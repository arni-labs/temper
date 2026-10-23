use super::*;

async fn owner_identity(mut request: Request<Body>, next: Next) -> axum::response::Response {
    let owner = request
        .headers()
        .get("x-test-owner")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("alice")
        .to_owned();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(&owner, "test-agent", None),
        ));
    next.run(request).await
}

#[tokio::test]
async fn queued_authorization_cannot_outlive_its_persisted_owner() {
    queued_owner_change(true).await;
}

#[tokio::test]
async fn non_strict_postgres_actions_acknowledge_queueing_not_execution() {
    queued_owner_change(false).await;
}

async fn queued_owner_change(strict: bool) {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
strict_action_params = true
[[state]]
name = "Notes"
type = "string"
initial = "alice"
[[state]]
name = "observations"
type = "counter"
initial = 0
[[action]]
name = "TransferOwner"
from = ["Draft"]
params = ["Notes"]
[[action]]
name = "Observe"
from = ["Draft"]
params = []
effect = ["observations += 1"]
[[action.triggers]]
name = "observed"
kind = "entity"
target_entity = "Sink"
target_action = "Record"
resolve_target = { kind = "same_id" }
"#
    .replace(
        "strict_action_params = true",
        &format!("strict_action_params = {strict}"),
    );
    let (pool, _container) = pool().await;
    let actors = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    actors
        .register(Arc::new(SpecDrivenActor::from_ioa(&spec).unwrap()))
        .await
        .unwrap();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Order", spec.as_str())],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-09-08T00:00:00Z".into(),
        }),
    );
    let mut state = ServerState::from_pg_registry(actors.clone(), registry);
    state.actor_backed_types.insert("Order".into());
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(principal == Agent::"alice", action, resource) when { resource.Notes == "alice" };
permit(principal == Agent::"bob", action, resource) when { resource.Notes == "bob" };
"#,
        )
        .unwrap();
    let router = build_router(state).layer(axum::middleware::from_fn(owner_identity));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let id = uuid::Uuid::new_v4().to_string();
    let handle = if strict {
        actors
            .spawn(&format!("default/{id}"), "Order")
            .await
            .unwrap()
    } else {
        actors
            .spawn_with_fields(&format!("default/{id}"), "Order", json!({"Notes":"alice"}))
            .await
            .unwrap()
    };
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    // The scheduler is stopped. Both requests see Alice's version, then we drive FIFO explicitly.
    for (action, body) in [
        ("TransferOwner", json!({"Notes":"bob"})),
        ("Observe", json!({})),
    ] {
        let response = client
            .post(format!("{base}/tdata/Orders('{id}')/Temper.{action}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "{}",
            response.text().await.unwrap()
        );
    }
    assert!(actors.activate_now(&handle).await.unwrap());
    let after_transfer = actor_state(&pool, &handle).await;
    let transfer: Value = serde_json::from_slice(&after_transfer).unwrap();
    assert_eq!(transfer["fields"]["Notes"], "bob");
    assert!(
        actors.activate_now(&handle).await.is_err(),
        "stale authorization executed"
    );
    assert_eq!(actor_state(&pool, &handle).await, after_transfer);
    assert_eq!(
        message_count(&pool, &handle.namespace).await,
        2,
        "refusal emitted a message"
    );
    assert!(
        !actors.activate_now(&handle).await.unwrap(),
        "refusal did not consume the message"
    );
    let response = client
        .post(format!("{base}/tdata/Orders('{id}')/Temper.Observe"))
        .header("x-test-owner", "bob")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(actors.activate_now(&handle).await.unwrap());
    let final_state: Value = serde_json::from_slice(&actor_state(&pool, &handle).await).unwrap();
    assert_eq!(final_state["counters"]["observations"], 1);
    assert_eq!(message_count(&pool, &handle.namespace).await, 4);
    server.abort();
}

/// The Postgres runtime writes every key of a non-strict action's body into
/// fields, so keys the action does not declare need the caller's update grant.
#[tokio::test]
async fn postgres_undeclared_keys_need_the_callers_update_grant() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
[[state]]
name = "Notes"
type = "string"
initial = "alice"
[[action]]
name = "Observe"
from = ["Draft"]
params = []
"#;
    let (pool, _container) = pool().await;
    let actors = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    actors
        .register(Arc::new(SpecDrivenActor::from_ioa(spec).unwrap()))
        .await
        .unwrap();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Order", spec)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-23T00:00:00Z".into(),
        }),
    );
    let mut state = ServerState::from_pg_registry(actors.clone(), registry);
    state.actor_backed_types.insert("Order".into());
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(principal, action == Action::"Observe", resource);
permit(principal, action == Action::"update", resource) when { resource.Notes == principal.id };
"#,
        )
        .unwrap();
    let router = build_router(state).layer(axum::middleware::from_fn(owner_identity));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let id = uuid::Uuid::new_v4().to_string();
    let handle = actors
        .spawn_with_fields(&format!("default/{id}"), "Order", json!({"Notes":"alice"}))
        .await
        .unwrap();
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    let observe = |who: &'static str, body: Value| {
        client
            .post(format!("{base}/tdata/Orders('{id}')/Temper.Observe"))
            .header("x-test-owner", who)
            .json(&body)
            .send()
    };

    let refused = observe("bob", json!({"Notes": "bob"})).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        message_count(&pool, &handle.namespace).await,
        0,
        "a refused call queues nothing"
    );

    let accepted = observe("bob", json!({})).await.unwrap();
    assert_eq!(
        accepted.status(),
        StatusCode::ACCEPTED,
        "declared params work as before"
    );

    let accepted = observe("alice", json!({"label": "kept"})).await.unwrap();
    assert_eq!(
        accepted.status(),
        StatusCode::ACCEPTED,
        "the owner may update"
    );
    assert_eq!(message_count(&pool, &handle.namespace).await, 2);
    server.abort();
}
