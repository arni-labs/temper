//! The Line and Shipment app the trigger tests run, and how they call it.
//! Each test that reads the refused-reaction counter is its own binary (the
//! meter is set once per process), and they share this.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::storage::StorageStack;
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

pub fn request(method: Method, uri: &str, body: Value) -> Request<Body> {
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
pub async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

pub async fn act(
    app: &axum::Router,
    set: &str,
    id: &str,
    action: &str,
    body: Value,
) -> (StatusCode, Value) {
    let uri = format!("/tdata/{set}('{id}')/Temper.{action}");
    send(app, request(Method::POST, &uri, body)).await
}

pub async fn read(app: &axum::Router, set: &str, id: &str) -> (StatusCode, Value) {
    send(
        app,
        request(Method::GET, &format!("/tdata/{set}('{id}')"), json!({})),
    )
    .await
}

/// How many reactions were counted as refused for `Shipment.Fix`, by reason.
pub fn refused(
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

/// A server for the Line and Shipment specs with four Lines made, on `store`
/// or, without one, in memory only.
pub async fn server(temp: &tempfile::TempDir, store: Option<TursoEventStore>) -> ServerState {
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
    if let Some(store) = store {
        state.set_storage_stack(StorageStack::from_turso(store));
    }
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
    for id in ["line-1", "line-2", "line-3", "line-4"] {
        state
            .get_or_create_tenant_entity(&TenantId::new(TENANT), "Line", id, json!({}))
            .await
            .unwrap();
    }
    state
}
