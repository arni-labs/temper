//! A trigger aimed at a record that does not exist fails where its sender
//! sees it, makes no record, and is counted.
//!
//! Before this, the kernel made an empty record for the id, refused the move
//! on it, and threw the refusal away: the sender got a clean answer and the
//! work the trigger was to start never started (Arni Ops, 19 Nov 2026: a
//! line's `Fix` sent to a shipment id that was a bare commit).
//!
//! Its own test binary, so the meter is set before anything in this process
//! records a metric.

#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const TENANT: &str = "default";

const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Line"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
<EntityType Name="Shipment"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
<EntityContainer Name="Container"><EntitySet Name="Lines" EntityType="Test.Line"/><EntitySet Name="Shipments" EntityType="Test.Shipment"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A line whose `Fail` sends `Fix` to the shipment its `shipment` names.
const LINE: &str = r#"
[automaton]
name = "Line"
states = ["Open", "Failed"]
initial = "Open"

[[state]]
name = "shipment"
type = "string"
initial = ""

[[action]]
name = "Fail"
kind = "input"
from = ["Open"]
to = "Failed"
params = ["shipment"]

[[action.triggers]]
name = "fix"
kind = "entity"
target_entity = "Shipment"
target_action = "Fix"
[action.triggers.resolve_target]
kind = "field"
id_field = "shipment"
"#;

/// A shipment is made by `Add`, and `Fix` works only on one taken back.
const SHIPMENT: &str = r#"
[automaton]
name = "Shipment"
states = ["Unmade", "Back", "Fixing"]
initial = "Unmade"

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Back"

[[action]]
name = "Fix"
kind = "input"
from = ["Back"]
to = "Fixing"
"#;

fn request(method: Method, uri: &str, body: Value) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            SecurityContext {
                principal: Principal {
                    id: "agent-1".to_string(),
                    kind: PrincipalKind::Customer,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    label: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "trigger-missing-record".to_string(),
                cause: None,
                credential: None,
            },
        ));
    request
}

/// Send `request` and answer its status and JSON body.
async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

async fn act(
    app: &axum::Router,
    set: &str,
    id: &str,
    action: &str,
    body: Value,
) -> (StatusCode, Value) {
    let uri = format!("/tdata/{set}('{id}')/Temper.{action}");
    send(app, request(Method::POST, &uri, body)).await
}

async fn read(app: &axum::Router, set: &str, id: &str) -> (StatusCode, Value) {
    send(
        app,
        request(Method::GET, &format!("/tdata/{set}('{id}')"), json!({})),
    )
    .await
}

/// How many reactions were counted as refused for `Shipment.Fix`, by reason.
fn refused(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
) -> std::collections::BTreeMap<String, u64> {
    provider.force_flush().unwrap();
    let mut newest = std::collections::BTreeMap::new();
    for batch in exporter.get_finished_metrics().unwrap() {
        for scope in batch.scope_metrics {
            for metric in scope
                .metrics
                .into_iter()
                .filter(|m| m.name == "temper_reaction_refused_total")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in &sum.data_points {
                    let label = |name: &str| {
                        point
                            .attributes
                            .iter()
                            .find(|kv| kv.key.as_str() == name)
                            .map(|kv| kv.value.as_str().to_string())
                            .unwrap_or_default()
                    };
                    assert_eq!(point.attributes.len(), 4, "identity belongs in the log");
                    assert_eq!(label("tenant"), TENANT);
                    assert_eq!(label("entity_type"), "Shipment");
                    assert_eq!(label("action"), "Fix");
                    newest.insert(label("reason"), point.value);
                }
            }
        }
    }
    newest
}

#[tokio::test]
async fn a_trigger_aimed_at_a_record_that_does_not_exist_fails_where_its_sender_sees_it() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let temp = tempfile::tempdir().expect("temporary test directory");
    let url = format!("file:{}", temp.path().join("metadata.db").display());
    let store = TursoEventStore::new(&url, None).await.expect("store opens");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        temper_spec::csdl::parse_csdl(CSDL).unwrap(),
        CSDL.to_string(),
        &[("Line", LINE), ("Shipment", SHIPMENT)],
    );
    for entity_type in ["Line", "Shipment"] {
        registry.set_verification_status(
            &TenantId::new(TENANT),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-11-19T00:00:00Z".to_string(),
            }),
        );
    }
    let mut state =
        ServerState::from_registry(ActorSystem::new("trigger-missing-record"), registry);
    state.data_dir = temp.path().join("data");
    state.set_storage_stack(StorageStack::from_turso(store));
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    for id in ["line-1", "line-2", "line-3"] {
        state
            .get_or_create_tenant_entity(&TenantId::new(TENANT), "Line", id, json!({}))
            .await
            .unwrap();
    }
    let app = build_router(state);

    // The line fails and names a shipment that was never made. Its own move
    // is committed; the trigger's failure is said beside it.
    let (status, failed) = act(
        &app,
        "Lines",
        "line-1",
        "Fail",
        json!({"shipment": "abc123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{failed}");
    assert_eq!(failed["status"], "Failed", "{failed}");
    let failures = failed["@temper.triggerFailures"]
        .as_array()
        .unwrap_or_else(|| panic!("the sender is told its trigger failed: {failed}"));
    assert_eq!(failures.len(), 1, "{failed}");
    let failure = failures[0].as_str().unwrap();
    assert!(failure.contains("fix"), "it names the trigger: {failure}");
    assert!(
        failure.contains("Shipment 'abc123' does not exist"),
        "it says the record is missing: {failure}"
    );
    // No empty record was made for the id.
    assert_eq!(
        read(&app, "Shipments", "abc123").await.0,
        StatusCode::NOT_FOUND
    );
    let counted = |reasons: &[(&str, u64)]| -> std::collections::BTreeMap<String, u64> {
        reasons
            .iter()
            .map(|(reason, count)| (reason.to_string(), *count))
            .collect()
    };
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 1)]),
        "and it is counted as a missing record"
    );

    // A request's own move on a record that does not exist is refused the
    // same way, and makes none either.
    let (status, body) = act(&app, "Shipments", "nowhere", "Fix", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "InvalidState", "{body}");
    assert_eq!(body["error"]["state"], "Unmade", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Shipment 'nowhere' does not exist")),
        "{body}"
    );
    assert_eq!(
        read(&app, "Shipments", "nowhere").await.0,
        StatusCode::NOT_FOUND
    );

    // A move that starts from the initial state still makes its record, and a
    // trigger that names a record that exists reaches it and says nothing.
    let (status, made) = act(&app, "Shipments", "made", "Add", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, reached) = act(&app, "Lines", "line-2", "Fail", json!({"shipment": "made"})).await;
    assert_eq!(status, StatusCode::OK, "{reached}");
    assert!(
        reached.get("@temper.triggerFailures").is_none(),
        "{reached}"
    );
    let (status, shipment) = read(&app, "Shipments", "made").await;
    assert_eq!(status, StatusCode::OK, "{shipment}");
    assert_eq!(shipment["status"], "Fixing", "{shipment}");
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 1)]),
        "nothing more was refused"
    );

    // A record that exists and is not where the move starts from refuses as
    // it always did, is said to the sender too, and is counted apart.
    let (status, again) = act(&app, "Lines", "line-3", "Fail", json!({"shipment": "made"})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    let failure = again["@temper.triggerFailures"][0]
        .as_str()
        .unwrap_or_else(|| panic!("the sender is told: {again}"));
    assert!(
        failure.contains("not valid from state 'Fixing'") && !failure.contains("does not exist"),
        "{failure}"
    );
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 1), ("state", 1)])
    );
}
