use super::*;

/// The Postgres actor merges an action's params into its fields and keeps
/// its counters apart, so a param named like a counter is stored as sent.
const COUNTED_SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
[[state]]
name = "Count"
type = "counter"
initial = 0
[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]
params = ["Count"]
effect = ["Count += 1"]
[[field_invariant]]
name = "CountStaysSmall"
message = "an order's count stays under 50"
assert = "Count < 50"
"#;

/// A Postgres-runtime server for `spec`'s Order, with one Order spawned.
struct Served {
    pool: Pool,
    _container: Option<ContainerAsync<Postgres>>,
    actors: Arc<ActorSystem>,
    handle: ActorHandle,
    server: tokio::task::JoinHandle<()>,
    action_url: String,
}

async fn serve(spec: &'static str) -> Served {
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
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let router = build_router(state).layer(axum::middleware::from_fn(test_identity));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let id = uuid::Uuid::new_v4().to_string();
    let handle = actors
        .spawn_with_fields(&format!("default/{id}"), "Order", json!({}))
        .await
        .unwrap();
    let action_url = format!("{base}/tdata/Orders('{id}')/Temper.AddItem");
    Served {
        pool,
        _container,
        actors,
        handle,
        server,
        action_url,
    }
}

#[tokio::test]
async fn a_postgres_action_is_checked_on_the_fields_its_actor_stores() {
    let Served {
        pool,
        _container,
        handle,
        server,
        action_url,
        ..
    } = serve(COUNTED_SPEC).await;
    let initial = actor_state(&pool, &handle).await;
    let client = Client::new();

    // The counter would read 1, but the actor stores the sent 100 in its fields.
    let response = client
        .post(&action_url)
        .json(&json!({"Count": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "{}",
        response.text().await.unwrap()
    );
    assert_eq!(message_count(&pool, &handle.namespace).await, 0);
    assert_eq!(actor_state(&pool, &handle).await, initial);

    let response = client
        .post(&action_url)
        .json(&json!({"Count": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "{}",
        response.text().await.unwrap()
    );
    assert_eq!(message_count(&pool, &handle.namespace).await, 1);
    server.abort();
}

/// The same order, kept under two items.
const UNDER_TWO_SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
[[state]]
name = "Count"
type = "counter"
initial = 0
[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]
effect = ["Count += 1"]
[[invariant]]
name = "UnderTwo"
assert = "Count < 2"
"#;

/// The actor keeps its counter apart from its fields; a rule that reads the
/// counter is checked on it: the second item would make it 2.
#[tokio::test]
async fn a_postgres_action_is_checked_on_the_counters_its_actor_keeps() {
    let Served {
        pool,
        _container,
        actors,
        handle,
        server,
        action_url,
    } = serve(UNDER_TWO_SPEC).await;
    let client = Client::new();
    let response = client
        .post(&action_url)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "{}",
        response.text().await.unwrap()
    );
    // The actor takes the first item: its counter is 1.
    assert!(actors.activate_now(&handle).await.unwrap());
    let queued = message_count(&pool, &handle.namespace).await;
    let response = client
        .post(&action_url)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "{}",
        response.text().await.unwrap()
    );
    assert_eq!(message_count(&pool, &handle.namespace).await, queued);
    server.abort();
}
