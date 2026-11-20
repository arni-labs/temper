//! An entity action takes a body up to what one integration may read back of it (16 MiB), and refuses one past it.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Page"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="text" Type="Edm.String" Nullable="false"/>
</EntityType><EntityContainer Name="Container"><EntitySet Name="Pages" EntityType="Temper.Page"/>
</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
const IOA: &str = r#"
[automaton]
name = "Page"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "text"
type = "string"
initial = ""
overflow_inline_max_bytes = 16777216
[[action]]
name = "Write"
params = [{ name = "text", type = "string" }]
kind = "input"
from = ["Ready"]
to = "Ready"
"#;

async fn post(state: &ServerState, path: &str, body: String) -> (StatusCode, Vec<u8>) {
    request(state, "POST", path, body).await
}

async fn request(
    state: &ServerState,
    method: &str,
    path: &str,
    body: String,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::from("default"),
            temper_authz::SecurityContext::from_resolved_identity("test", "test", None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn an_action_takes_a_body_past_two_megabytes_and_refuses_one_past_sixteen_mebibytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("body.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state =
        ServerState::from_registry(ActorSystem::new("tdata-body-limit"), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store));
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.to_string(),
            &[("Page", IOA)],
        );
        registry.set_verification_status(
            &TenantId::from("default"),
            "Page",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-11-20T00:00:00Z".into(),
            }),
        );
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let (status, body) = post(&state, "/tdata/Pages", json!({"id": "p1"}).to_string()).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // 3 MiB: past axum's default of 2 MB, which this route had before, and well within 16 MiB.
    let text = "a".repeat(3 * 1024 * 1024);
    let (status, body) = post(
        &state,
        "/tdata/Pages('p1')/Temper.Write",
        json!({"text": text}).to_string(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body[..body.len().min(500)])
    );
    let (status, body) = request(&state, "GET", "/tdata/Pages('p1')", String::new()).await;
    assert_eq!(status, StatusCode::OK);
    let read: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        read["fields"]["text"].as_str().map(str::len),
        Some(text.len()),
        "the whole value is kept"
    );

    // Past 16 MiB: refused as too large, nothing written.
    let (status, _) = post(
        &state,
        "/tdata/Pages('p1')/Temper.Write",
        json!({"text": "b".repeat(16 * 1024 * 1024 + 1)}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
