//! A question must not enqueue approval work; a refused dispatch still must.
#![cfg(feature = "observe")]

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{persistence::EventStore, tenant::TenantId};
use temper_server::{StorageStack, build_router};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

async fn ask(
    app: &Router,
    path: &str,
    body: Value,
    tenant: &str,
    claimed: &str,
) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(
        AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity(claimed, "offer-agent", None),
        )
        .with_session_id(Some("offers-session".into())),
    );
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 2_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn single_offer_question_files_no_decision_but_refused_dispatch_files_one() {
    for seed in [26, 891, 1770] {
        let (mut state, _) = common::build_default_state(seed, "offer-check");
        let temp = tempfile::tempdir().unwrap();
        let store = TursoEventStore::new(
            &format!("file:{}", temp.path().join("store.db").display()),
            None,
        )
        .await
        .unwrap();
        state.set_storage_stack(StorageStack::from_turso(store.clone()));
        let system_csdl = include_str!("../../temper-platform/src/specs/model.csdl.xml");
        state.registry.write().unwrap().register_tenant(
            "temper-system",
            temper_spec::csdl::parse_csdl(system_csdl).unwrap(),
            system_csdl.to_string(),
            &[(
                "GovernanceDecision",
                include_str!("../../temper-platform/src/specs/GovernanceDecision.ioa.toml"),
            )],
        );
        common::dispatch(
            &state,
            &TenantId::default(),
            "Order",
            "order-1",
            "AddItem",
            json!({"ProductId":"p", "Quantity":1}),
        )
        .await
        .unwrap();
        state
            .authz
            .reload_tenant_policies(
                "default",
                r#"
            permit(principal is Customer, action, resource is Order);
        "#,
            )
            .unwrap();
        let mut decisions = state.pending_decision_tx.subscribe();
        let app = build_router(state);
        let (status, answer) = ask(&app, "/api/authorize", json!({
            "agent_id":"offer-agent", "resource_type":"Order", "resource_id":"order-1", "action":"AddItem"
        }), "default", "offer-agent").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["allowed"], false);
        assert!(answer["reason"].as_str().is_some());
        assert_eq!(
            store.query_all_decisions(None).await.unwrap().len(),
            0,
            "an offer question must not file a pending decision"
        );
        assert!(answer.get("decision_id").is_none());
        let (status, batch) = ask(
            &app,
            "/api/authorize",
            json!({
                "agent_id":"offer-agent", "records":[{
                    "resource_type":"Order", "resource_id":"order-1", "context":{"status":"Draft"},
                    "actions":["AddItem", "SubmitOrder", "ConfirmOrder"]
                }]
            }),
            "default",
            "offer-agent",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let offers = batch["records"][0]["actions"].as_array().unwrap();
        assert_eq!(offers.len(), 3);
        for offer in offers {
            assert_eq!(offer["allowed"], false);
            assert!(offer["reason"].as_str().is_some());
            assert!(offer.get("decision_id").is_none());
        }
        assert!(
            matches!(
                decisions.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "a question must not broadcast approval work"
        );
        assert!(store.query_all_decisions(None).await.unwrap().is_empty());
        assert!(
            store
                .list_entity_ids_by_type("temper-system", "GovernanceDecision")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .load_policy_denial_patterns("default")
                .await
                .unwrap()
                .is_empty()
        );
        let (status, answer) = ask(
            &app,
            "/tdata/Orders('order-1')/Temper.AddItem",
            json!({}),
            "default",
            "offer-agent",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
        let decision = decisions
            .try_recv()
            .expect("refused dispatch must file its decision");
        assert_eq!(answer["decision_id"], decision.id);
        assert_eq!(decision.action, "AddItem");
        assert!(
            matches!(
                decisions.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "exactly one dispatch decision"
        );
        assert_eq!(store.query_all_decisions(None).await.unwrap().len(), 1);
        assert_eq!(
            store
                .list_entity_ids_by_type("temper-system", "GovernanceDecision")
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn factory_sized_batch_matches_individual_cedar_questions_and_shares_context() {
    let (state, _) = common::build_default_state(100, "batch-offers");
    state.authz.reload_tenant_policies("default", r#"
        permit(principal == Agent::"offer-agent", action == Action::"Move0", resource == Order::"release-1")
        when { resource.status == "Draft" };
        permit(principal is Customer, action, resource is Order);
    "#).unwrap();
    let mut decisions = state.pending_decision_tx.subscribe();
    let app = build_router(state);
    let actions: Vec<String> = (0..100).map(|n| format!("Move{n}")).collect();
    let context = json!({"status":"Draft", "id":"forged"});
    let mut expected = Vec::new();
    for action in &actions {
        let (status, answer) = ask(&app, "/api/authorize", json!({
            "agent_id":"offer-agent", "resource_type":"Order", "resource_id":"release-1", "action":action, "context":context
        }), "default", "offer-agent").await;
        assert_eq!(status, StatusCode::OK);
        expected
            .push(json!({"action":action, "allowed":answer["allowed"], "reason":answer["reason"]}));
    }
    let batch = json!({"agent_id":"offer-agent", "records":[
        {"resource_type":"Order", "resource_id":"release-1", "actions":actions, "context":context},
        {"resource_type":"Order", "resource_id":"release-2", "actions":["Move0"], "context":context}
    ]});
    let (status, answer) = ask(
        &app,
        "/api/authorize",
        batch.clone(),
        "default",
        "offer-agent",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["records"][0]["actions"], json!(expected));
    assert_eq!(answer["records"][0]["actions"][0]["allowed"], true);
    assert_eq!(answer["records"][1]["actions"][0]["allowed"], false);
    assert!(matches!(
        decisions.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let (status, _) = ask(&app, "/api/authorize", batch.clone(), "default", "impostor").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, answer) = ask(&app, "/api/authorize", batch, "other", "offer-agent").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["records"][0]["actions"][0]["allowed"], false);
    let (status, _) = ask(
        &app,
        "/api/authorize",
        json!({"agent_id":"offer-agent", "records":[
            {"resource_type":"Order", "resource_id":"release-1", "actions":["Move0"], "context":[]}
        ]}),
        "default",
        "offer-agent",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
