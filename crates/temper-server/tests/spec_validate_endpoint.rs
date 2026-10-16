#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::{ServerState, SpecRegistry, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn test_state_with_registry() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    ServerState::from_registry(ActorSystem::new("spec-validate-endpoint-test"), registry)
}

fn authenticated_post(uri: &str, body: &str) -> Request<Body> {
    let mut request = Request::post(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::system(),
        ));
    request
}

#[tokio::test]
async fn validate_ioa_runs_server_cascade_without_loading_spec() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({
                "ioa_source": ORDER_IOA,
                "sim_seeds": 1,
                "prop_test_cases": 10
            })
            .to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["all_passed"], true);
}

#[tokio::test]
async fn validate_ioa_rejects_empty_source() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": "" }).to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A spec whose model check runs for minutes with no budget.
const HEAVY_FEED: &str = include_str!("../../../test-fixtures/budget/heavy_feed.toml");

#[tokio::test]
async fn validate_ioa_answers_a_spec_too_large_to_check_within_its_budget() {
    let app = build_router(test_state_with_registry());
    let started = std::time::Instant::now();
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": HEAVY_FEED, "budget_ms": 1000 }).to_string(),
        ))
        .await
        .unwrap();
    let took = started.elapsed();

    assert_eq!(response.status(), StatusCode::OK);
    // 1 s of budget, then a queue that no longer grows drains; 10 s leaves
    // room for a loaded CI machine, against the minutes it takes unbudgeted.
    assert!(
        took < std::time::Duration::from_secs(10),
        "answered in {took:?}"
    );
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["all_passed"], false);
    let stopped = json["stopped"]
        .as_str()
        .expect("stopped names the type and why");
    assert!(
        stopped.starts_with("HeavyFeed: L1 Model Check STOPPED: ran out of its 1 s budget"),
        "{stopped}"
    );
}

#[tokio::test]
async fn validate_ioa_rejects_a_budget_out_of_range() {
    for budget_ms in [0, 120_001] {
        let app = build_router(test_state_with_registry());
        let response = app
            .oneshot(authenticated_post(
                "/api/specs/validate-ioa",
                &serde_json::json!({ "ioa_source": ORDER_IOA, "budget_ms": budget_ms }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{budget_ms}");
    }
}

#[tokio::test]
async fn validate_ioa_answers_a_move_with_too_many_parameter_choices() {
    let flags: Vec<String> = (0..13).map(|i| format!("f{i}")).collect();
    let ioa = format!(
        "[automaton]\nname = \"Flags\"\nstates = [\"On\"]\ninitial = \"On\"\n\n{}\n[[action]]\nname = \"Set\"\nkind = \"input\"\nfrom = [\"On\"]\nparams = [{}]\neffect = [{}]\n",
        flags
            .iter()
            .map(|f| format!("[[state]]\nname = \"{f}\"\ntype = \"bool\"\ninitial = false\n"))
            .collect::<Vec<_>>()
            .join("\n"),
        flags
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", "),
        flags
            .iter()
            .map(|f| format!("\"{f} = params.{f}\""))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": ioa }).to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an answer about the spec, not a worker panic"
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let stopped = json["stopped"]
        .as_str()
        .expect("stopped names the type and why");
    assert!(
        stopped.starts_with("Flags: L1 Model Check FAILED: Set takes 8192"),
        "{stopped}"
    );
}
