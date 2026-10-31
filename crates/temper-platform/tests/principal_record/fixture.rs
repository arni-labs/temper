//! A user's Temper with a trusted issuer, an app's `Member` type marked
//! `principal_record`, and a `Note` the app's Cedar opens to members. Every
//! call goes through the platform router with a real ES256 token, so the
//! bearer boundary, the resolver and Cedar all run as in production.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const ISSUER: &str = "https://issuer.principal-record.local";
const AUD: &str = "temper-principal-record";
const NOTE: &str = "n-1";

/// A member of the app.
pub const ALICE: &str = "user-alice";
/// A user with no Member record.
pub const BOB: &str = "user-bob";

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.PrincipalRecord" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Member">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="Member" Type="Edm.String"/>
        <Property Name="actingForMember" Type="Edm.String"/>
      </EntityType>
      <Action Name="Touch" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.PrincipalRecord.Note"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Members" EntityType="Temper.PrincipalRecord.Member"/>
        <EntitySet Name="Notes" EntityType="Temper.PrincipalRecord.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// One record per user, keyed by the user's `sub`.
const MEMBER_IOA: &str = r#"
[automaton]
name = "Member"
states = ["Active", "Removed"]
initial = "Active"
allow_indefinite_states = ["Active", "Removed"]
principal_record = true

[[action]]
name = "Remove"
kind = "input"
from = ["Active"]
to = "Removed"

[[action]]
name = "Rejoin"
kind = "input"
from = ["Removed"]
to = "Active"
"#;

/// Its fields carry the attribute names, as a user who writes a note could.
const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[state]]
name = "Member"
type = "string"
initial = ""

[[state]]
name = "actingForMember"
type = "string"
initial = ""

[[action]]
name = "Touch"
kind = "input"
from = ["Open"]
to = "Open"
"#;

/// Reading is for a member in person; `Touch` also for an agent acting for one.
const POLICY: &str = r#"
permit(principal, action == Action::"list", resource is Note);
permit(principal, action == Action::"read", resource is Note)
when { principal has Member && principal.Member == "Active" };
permit(principal, action == Action::"Touch", resource is Note)
when {
    (principal has Member && principal.Member == "Active") ||
    (principal has actingForMember && principal.actingForMember == "Active")
};
"#;

fn signing_key() -> SigningKey {
    SigningKey::from_slice(&[11u8; 32]).expect("a P-256 key")
}

fn b64(value: &serde_json::Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
}

fn mint(claims: serde_json::Value) -> String {
    let header = serde_json::json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" });
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let signature: Signature = signing_key().sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

fn jwks_json() -> String {
    let point = signing_key().verifying_key().to_encoded_point(false);
    serde_json::json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
        "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
    }]})
    .to_string()
}

/// The user `sub`, signed in as themselves.
pub fn in_person(sub: &str) -> String {
    mint(serde_json::json!({
        "iss": ISSUER, "aud": AUD, "sub": sub, "nbf": 0, "exp": 4_102_444_800i64,
    }))
}

/// An agent acting for the user `sub`.
pub fn agent_for(sub: &str) -> String {
    mint(serde_json::json!({
        "iss": ISSUER, "aud": AUD, "sub": sub, "client_id": format!("agent-of-{sub}"),
        "agent_type": "assistant", "nbf": 0, "exp": 4_102_444_800i64,
    }))
}

fn tenant() -> TenantId {
    TenantId::new("default")
}

/// The app on the simulated journal, with a note whose fields claim
/// membership and Alice an Active member.
pub async fn app(seed: u64) -> PlatformState {
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
    let cache = BTreeMap::new();
    bootstrap_system_tenant(&state, &cache);
    bootstrap_agent_specs(&state, "default", false, &cache);
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .try_register_tenant_with_constraints(
                tenant(),
                parse_csdl(CSDL).expect("app CSDL parses"),
                CSDL.to_string(),
                &[("Member", MEMBER_IOA), ("Note", NOTE_IOA)],
                None,
                true,
            )
            .expect("the app's specs merge into the tenant");
        for entity_type in ["Member", "Note"] {
            registry.set_verification_status(
                &tenant(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "fixture".to_string(),
                        passed: true,
                        summary: "pre-verified".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-10-28T00:00:00Z".to_string(),
                }),
            );
        }
    }
    let system = AgentContext::system();
    state
        .server
        .dispatch_tenant_action(
            &tenant(),
            "TrustedIssuer",
            ISSUER,
            "RegisterIssuer",
            serde_json::json!({
                "issuer": ISSUER, "jwks_json": jwks_json(), "audience": AUD,
                "algorithms": "ES256", "description": "principal record", "created_by": "test",
            }),
            &system,
        )
        .await
        .expect("register the issuer");
    state
        .server
        .get_or_create_tenant_entity(
            &tenant(),
            "Note",
            NOTE,
            serde_json::json!({ "Member": "Active", "actingForMember": "Active" }),
        )
        .await
        .expect("create the note");
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Member", ALICE, serde_json::json!({}))
        .await
        .expect("Alice joins");
    state
        .server
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("the app's Cedar parses");
    state
}

/// Move `sub`'s Member record (`Remove`, `Rejoin`); its new status.
pub async fn move_member(state: &PlatformState, sub: &str, action: &str) -> String {
    let response = state
        .server
        .dispatch_tenant_action(
            &tenant(),
            "Member",
            sub,
            action,
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .unwrap_or_else(|error| panic!("{action} {sub}: {error}"));
    assert!(response.success, "{action} {sub}: {:?}", response.error);
    response.state.status
}

/// Let `sub`'s Member actor go idle past the timeout and passivate it.
pub async fn passivate_member(state: &PlatformState, sub: &str) {
    let actor_key = format!("{}:Member:{sub}", tenant());
    if let Ok(mut last_accessed) = state.server.last_accessed.write() {
        last_accessed.insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );
    }
    state.server.passivate_idle_actors().await;
    assert!(
        !state
            .server
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "the Member actor is passivated"
    );
}

async fn call(state: &PlatformState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = build_platform_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// `GET /tdata/Notes('n-1')` with `token`.
pub async fn read_note(state: &PlatformState, token: &str) -> StatusCode {
    let request = Request::get(format!("/tdata/Notes('{NOTE}')"))
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    call(state, request).await.0
}

/// `POST /tdata/Notes('n-1')/Temper.PrincipalRecord.Touch` with `token`.
pub async fn touch_note(state: &PlatformState, token: &str) -> StatusCode {
    let request = Request::post(format!(
        "/tdata/Notes('{NOTE}')/Temper.PrincipalRecord.Touch"
    ))
    .header("Authorization", format!("Bearer {token}"))
    .header("Content-Type", "application/json")
    .body(Body::from("{}"))
    .unwrap();
    call(state, request).await.0
}

/// `GET /tdata/Notes` with `token`: the status and the ids listed.
pub async fn list_notes(state: &PlatformState, token: &str) -> (StatusCode, Vec<String>) {
    let request = Request::get("/tdata/Notes")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(state, request).await;
    let ids = body["value"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["entity_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (status, ids)
}
