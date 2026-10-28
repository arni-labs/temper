//! Each `$filter` the OData route accepts returns exactly the records it
//! names, read through the `/tdata` router from a server over a real store
//! (a Turso file, and Postgres where `DATABASE_URL` is set).
//!
//! The route narrows a filter to candidate rows in SQL where it can (string
//! and boolean equality, `Status` ranges, and `and`/`or` of those) and decides
//! the rest on the records themselves, so every case here passes through
//! one path or the other. The reads start once the store's projection has
//! caught up with every write, so the SQL rows are what the route reads.
//! An exact match still finds a record whose projection fell behind its
//! journal. A filter that does not parse is refused with where it went wrong.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::projection_writes_fail::ProjectionWritesFail;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::storage::QueryPlaneStore;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Parts" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Part">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="Color" Type="Edm.String"/>
        <Property Name="Size" Type="Edm.Int32"/>
        <Property Name="Weight" Type="Edm.Double"/>
        <Property Name="InStock" Type="Edm.Boolean"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Parts" EntityType="Temper.Parts.Part"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const PART_IOA: &str = r#"
[automaton]
name = "Part"
states = ["Draft", "Live", "Retired"]
initial = "Draft"

[[action]]
name = "Publish"
kind = "input"
from = ["Draft"]
to = "Live"

[[action]]
name = "Retire"
kind = "input"
from = ["Live"]
to = "Retired"
"#;

/// The records every case reads, and the actions that move each one to its
/// status. `p3` holds an explicit null `Color`, `p5` none at all; `p4` and
/// `p6` have no `InStock`; only `p1`, `p2` and `p4` have a `Weight`.
fn parts() -> Vec<(Value, &'static [&'static str])> {
    vec![
        (
            json!({ "id": "p1", "Name": "bolt", "Color": "red", "Size": 3, "Weight": 1.5, "InStock": true }),
            &[],
        ),
        (
            json!({ "id": "p2", "Name": "nut", "Color": "blue", "Size": 10, "Weight": 0.25, "InStock": false }),
            &["Publish"],
        ),
        (
            json!({ "id": "p3", "Name": "O'Brien's washer", "Color": null, "Size": 7, "InStock": true }),
            &["Publish"],
        ),
        (
            json!({ "id": "p4", "Name": "50%_off gear", "Color": "red", "Size": -2, "Weight": 12.5 }),
            &["Publish", "Retire"],
        ),
        (
            json!({ "id": "p5", "Name": "spring", "Size": 10, "InStock": false }),
            &[],
        ),
        (
            json!({ "id": "p6", "Name": "red bolt", "Color": "Red", "Size": 3 }),
            &["Publish"],
        ),
    ]
}

const ALL: &[&str] = &["p1", "p2", "p3", "p4", "p5", "p6"];

/// Each filter and exactly the records it returns. Statuses: p1 and p5
/// Draft; p2, p3 and p6 Live; p4 Retired.
const CASES: &[(&str, &[&str])] = &[
    // Equality on strings and booleans, either way round.
    ("Color eq 'red'", &["p1", "p4"]),
    ("'red' eq Color", &["p1", "p4"]),
    ("Name eq 'O''Brien''s washer'", &["p3"]),
    ("Name eq '50%_off gear'", &["p4"]),
    ("Color eq 'green'", &[]),
    ("InStock eq true", &["p1", "p3"]),
    ("InStock eq false", &["p2", "p5"]),
    // `ne` and nulls: a missing property is null, and null is never another value.
    ("Color ne 'red'", &["p2", "p6"]),
    ("Color eq null", &["p3", "p5"]),
    ("Color ne null", &["p1", "p2", "p4", "p6"]),
    ("InStock ne true", &["p2", "p5"]),
    ("Size eq null", &[]),
    // Numbers compare as numbers, not as text.
    ("Size eq 10", &["p2", "p5"]),
    ("Size gt 3", &["p2", "p3", "p5"]),
    ("Size ge 3", &["p1", "p2", "p3", "p5", "p6"]),
    ("Size lt 3", &["p4"]),
    ("Size le 3", &["p1", "p4", "p6"]),
    ("Size gt -5", ALL),
    ("3 lt Size", &["p2", "p3", "p5"]),
    ("Weight lt 1.5", &["p2"]),
    ("Weight ge 1.5", &["p1", "p4"]),
    // The lifecycle status, by value and by order.
    ("Status eq 'Live'", &["p2", "p3", "p6"]),
    ("Status ne 'Live'", &["p1", "p4", "p5"]),
    ("Status gt 'Draft'", &["p2", "p3", "p4", "p6"]),
    ("Status ge 'Live'", &["p2", "p3", "p4", "p6"]),
    ("Status lt 'Live'", &["p1", "p5"]),
    ("Status le 'Draft'", &["p1", "p5"]),
    ("'Live' lt Status", &["p4"]),
    ("status eq 'Live'", &["p2", "p3", "p6"]),
    ("Status eq null", &[]),
    ("Status ne null", ALL),
    // String functions; `%` and `_` are plain characters.
    ("contains(Name,'bolt')", &["p1", "p6"]),
    ("startswith(Name,'bolt')", &["p1"]),
    ("endswith(Name,'bolt')", &["p1", "p6"]),
    ("startswith(Name,'50%_')", &["p4"]),
    ("contains(Name,'%')", &["p4"]),
    ("contains(Name,'_')", &["p4"]),
    ("endswith(Name,'''s washer')", &["p3"]),
    ("contains(Color,'e')", &["p1", "p2", "p4", "p6"]),
    // `and` binds tighter than `or`, `not` tighter than both; parentheses group.
    ("Color eq 'red' and Status eq 'Draft'", &["p1"]),
    ("Color eq 'red' or Color eq 'blue'", &["p1", "p2", "p4"]),
    (
        "Status eq 'Live' or Status eq 'Retired'",
        &["p2", "p3", "p4", "p6"],
    ),
    (
        "Color eq 'blue' or Color eq 'red' and Status eq 'Draft'",
        &["p1", "p2"],
    ),
    (
        "(Color eq 'blue' or Color eq 'red') and Status eq 'Retired'",
        &["p4"],
    ),
    ("not (Color eq 'red')", &["p2", "p3", "p5", "p6"]),
    ("not Color eq 'red' and Size eq 3", &["p6"]),
    (
        "not (Color eq 'red' and Size eq 3)",
        &["p2", "p3", "p4", "p5", "p6"],
    ),
    // Mixes of what SQL can narrow and what it cannot.
    ("Status ne 'Draft' and Color eq 'red'", &["p4"]),
    ("Color eq 'red' or Size eq 10", &["p1", "p2", "p4", "p5"]),
    ("Name eq 'bolt' and Size gt 1", &["p1"]),
    ("InStock eq true and Status eq 'Live'", &["p3"]),
    ("Name eq 'bolt' and Status eq 'Live'", &[]),
    ("Name eq Color", &[]),
];

/// Filters that do not parse, and what the refusal says went wrong.
const MALFORMED: &[(&str, &str)] = &[
    ("Color eq", "unexpected end of filter expression"),
    ("Color eq 'red' and", "unexpected end of filter expression"),
    ("Name eq 'bolt", "unterminated string literal"),
    ("Color eq 'red')", "unexpected token ')' after expression"),
    ("(Color eq 'red'", "expected ')', found end of input"),
    ("Color eq $x", "unexpected character '$'"),
    ("Size eq 1.2.3", "invalid float literal '1.2.3'"),
];

/// A server over `stack` for `tenant`, and the store's query projection.
fn server(stack: StorageStack, tenant: &str) -> (Router, Arc<dyn QueryPlaneStore>) {
    let projection = stack
        .query_plane
        .clone()
        .expect("the store keeps a query projection");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant,
        parse_csdl(CSDL_XML).expect("CSDL parses"),
        CSDL_XML.to_string(),
        &[("Part", PART_IOA)],
    );
    // HTTP writes wait for a spec's verification; mark it passed.
    registry.set_verification_status(
        &TenantId::from(tenant),
        "Part",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-28T00:00:00Z".to_string(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new("odata-filter-records"), registry);
    state.set_storage_stack(stack);
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .expect("policy loads");
    (build_router(state), projection)
}

/// The answer's status and body, for a request from a reader of `tenant`.
async fn send(app: &Router, tenant: &str, mut request: Request<Body>) -> (StatusCode, Value) {
    let security_context = temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "filter-reader".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "odata-filter-records".to_string(),
        cause: None,
    };
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::from(tenant),
            security_context,
        ));
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("the answer's body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(app: &Router, tenant: &str, path: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, tenant, request).await
}

/// Create every part over OData and move it to its status.
async fn seed(app: &Router, tenant: &str) {
    for (fields, actions) in parts() {
        let id = fields["id"].as_str().expect("a part has an id").to_string();
        let (status, body) = post(app, tenant, "/tdata/Parts", &fields).await;
        assert_eq!(status, StatusCode::CREATED, "create {id}: {body}");
        for action in actions {
            let path = format!("/tdata/Parts('{id}')/Temper.Parts.{action}");
            let (status, body) = post(app, tenant, &path, &json!({})).await;
            assert_eq!(status, StatusCode::OK, "{action} {id}: {body}");
        }
    }
}

/// Wait until the projection holds a current row for every part, so a read
/// narrows in SQL on rows that hold every write.
async fn wait_for_projection(projection: &dyn QueryPlaneStore, tenant: &str) {
    let mut current = BTreeSet::new();
    for _ in 0..400 {
        let rows = projection
            .projection_catalog_freshness(tenant, "Part")
            .await
            .expect("read the projection")
            .expect("the store compares its projection with the journal");
        current = rows
            .into_iter()
            .filter(|row| row.current)
            .map(|row| row.entity_id)
            .collect();
        if current.len() == ALL.len() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the projection did not catch up: current rows {current:?}");
}

/// Percent-encode a query value: everything but letters, digits and `-._~`.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

async fn read(app: &Router, tenant: &str, filter: &str) -> (StatusCode, Value) {
    let request = Request::get(format!("/tdata/Parts?$filter={}", encode(filter)))
        .body(Body::empty())
        .unwrap();
    send(app, tenant, request).await
}

/// Seed the parts, then check every case; report every wrong case at once.
async fn every_filter_returns_exactly_its_records(stack: StorageStack, tenant: &str) {
    let (app, projection) = server(stack, tenant);
    seed(&app, tenant).await;
    wait_for_projection(projection.as_ref(), tenant).await;

    let mut wrong = Vec::new();
    for (filter, expected) in CASES {
        let (status, body) = read(&app, tenant, filter).await;
        let returned: BTreeSet<&str> = body["value"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value["entity_id"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        let expected: BTreeSet<&str> = expected.iter().copied().collect();
        if status != StatusCode::OK || returned != expected {
            wrong.push(format!(
                "{filter}: wanted {expected:?}, got {status} {returned:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "wrong records:\n{}", wrong.join("\n"));
}

#[tokio::test]
async fn every_filter_returns_exactly_its_records_from_turso() {
    let dir = tempfile::tempdir().expect("a database of its own");
    let url = format!("file:{}", dir.path().join("parts.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("open the Turso file");
    every_filter_returns_exactly_its_records(StorageStack::from_turso(store), "default").await;
}

#[tokio::test]
async fn every_filter_returns_exactly_its_records_from_postgres() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to Postgres");
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .expect("migrate Postgres");
    let store = temper_store_postgres::PostgresEventStore::new(pool.clone());
    // A tenant of its own keeps this run apart from others on the database.
    let tenant = format!("filters-{}", uuid::Uuid::new_v4().simple());
    every_filter_returns_exactly_its_records(StorageStack::from_postgres(store), &tenant).await;
    pool.close().await;
}

#[tokio::test]
async fn an_exact_match_finds_a_record_whose_projection_fell_behind() {
    let dir = tempfile::tempdir().expect("a database of its own");
    let url = format!("file:{}", dir.path().join("behind.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("open the Turso file");
    let (stack, failing) = ProjectionWritesFail::stack(store);
    let (app, projection) = server(stack, "default");
    seed(&app, "default").await;
    wait_for_projection(projection.as_ref(), "default").await;

    // From here the journal takes every write and the projection none, so
    // the SQL rows still say p5 has no Color.
    failing.store(true, Ordering::SeqCst);
    let request = Request::builder()
        .method(Method::PATCH)
        .uri("/tdata/Parts('p5')")
        .header("Content-Type", "application/json")
        .body(Body::from(json!({ "Color": "green" }).to_string()))
        .unwrap();
    let (status, body) = send(&app, "default", request).await;
    assert_eq!(status, StatusCode::OK, "PATCH p5: {body}");

    for filter in ["Color eq 'green'", "Color eq 'green' and Status eq 'Draft'"] {
        let (status, body) = read(&app, "default", filter).await;
        assert_eq!(status, StatusCode::OK, "{filter}: {body}");
        let returned: Vec<&str> = body["value"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value["entity_id"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(returned, ["p5"], "{filter}: {body}");
    }
}

#[tokio::test]
async fn a_filter_that_does_not_parse_is_refused_with_where_it_went_wrong() {
    let dir = tempfile::tempdir().expect("a database of its own");
    let url = format!("file:{}", dir.path().join("malformed.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("open the Turso file");
    let (app, _) = server(StorageStack::from_turso(store), "default");

    let mut wrong = Vec::new();
    for (filter, reason) in MALFORMED {
        let (status, body) = read(&app, "default", filter).await;
        let message = body["error"]["message"].as_str().unwrap_or_default();
        if status != StatusCode::BAD_REQUEST
            || body["error"]["code"] != "InvalidQuery"
            || !message.starts_with("Invalid $filter at position")
            || !message.contains(reason)
        {
            wrong.push(format!(
                "{filter}: wanted 400 '{reason}', got {status} {body}"
            ));
        }
    }
    assert!(wrong.is_empty(), "wrong refusals:\n{}", wrong.join("\n"));
}
