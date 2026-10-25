//! A record's move history read through OData:
//! `GET /tdata/{Set}('{id}')/$history?after=<seq>&limit=<n>`.
//!
//! The history comes from the durable journal, not the actor's in-memory
//! event list (which only holds events since the last snapshot), it is paged
//! by stored sequence number, and it is authorized by the record's own Cedar
//! read.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::{ServerState, StorageStack};
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const ORDER_READ_POLICY: &str = r#"
    permit(
        principal,
        action in [Action::"list", Action::"read", Action::"delete"],
        resource is Order
    );
"#;

fn authenticate(mut request: Request<Body>, principal_id: &str) -> Request<Body> {
    let security_context = temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: principal_id.to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "record-history-test".to_string(),
    };
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    request
}

async fn request_json(
    state: &ServerState,
    method: Method,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let request = authenticate(
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap(),
        "customer-1",
    );
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn get_json(state: &ServerState, path: &str) -> (StatusCode, serde_json::Value) {
    request_json(state, Method::GET, path).await
}

fn install_policy(state: &ServerState, policy: &str) {
    state
        .authz
        .reload_tenant_policies(TenantId::default().as_str(), policy)
        .expect("install Cedar policy");
}

async fn create_order(state: &ServerState, id: &str) {
    common::dispatch(
        state,
        &TenantId::default(),
        "Order",
        id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create order");
}

async fn add_item(state: &ServerState, id: &str, n: usize) {
    let response = common::dispatch(
        state,
        &TenantId::default(),
        "Order",
        id,
        "AddItem",
        serde_json::json!({
            "ProductId": format!("p-{n}"),
            "Quantity": n,
            "moved_via": format!("channel-{n}"),
        }),
    )
    .await
    .expect("AddItem dispatch");
    assert!(response.success, "AddItem {n} failed: {:?}", response.error);
}

/// The stored sequence numbers and actions of an entity's journal, read
/// straight from the store: the ground truth the route must match.
async fn journal(store: &impl EventStore, id: &str) -> Vec<(u64, String)> {
    store
        .read_events(&format!("{}:Order:{id}", TenantId::default()), 0)
        .await
        .expect("read journal")
        .into_iter()
        .map(|envelope| (envelope.sequence_nr, envelope.event_type))
        .collect()
}

/// Walk every page of a record's history, following `next`.
async fn read_all_pages(
    state: &ServerState,
    id: &str,
    limit: usize,
) -> Vec<Vec<serde_json::Value>> {
    let mut pages = Vec::new();
    let mut after: Option<u64> = None;
    loop {
        let path = match after {
            Some(after) => format!("/tdata/Orders('{id}')/$history?after={after}&limit={limit}"),
            None => format!("/tdata/Orders('{id}')/$history?limit={limit}"),
        };
        let (status, body) = get_json(state, &path).await;
        assert_eq!(status, StatusCode::OK, "history page failed: {body:?}");
        let events = body["events"].as_array().expect("events array").clone();
        assert!(
            events.len() <= limit,
            "page holds {} events, more than limit {limit}",
            events.len()
        );
        pages.push(events);
        match body["next"].as_u64() {
            Some(next) => after = Some(next),
            None => {
                assert!(body["next"].is_null(), "next must be null on the last page");
                break;
            }
        }
        assert!(pages.len() < 1_000, "history paging did not terminate");
    }
    pages
}

fn sequences_and_actions(events: &[serde_json::Value]) -> Vec<(u64, String)> {
    events
        .iter()
        .map(|event| {
            (
                event["sequence"].as_u64().expect("stored sequence"),
                event["action"].as_str().expect("action").to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn history_spans_a_snapshot_boundary() {
    let seed = 5001;
    let (_guard, _clock, _ids) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let state = common::build_default_state_with_store(store.clone(), "history-snapshot");
    install_policy(&state, ORDER_READ_POLICY);
    let id = "ord-history-snapshot";

    create_order(&state, id).await;
    for n in 1..=4 {
        add_item(&state, id, n).await;
    }

    // Passivation snapshots the actor and stops it; the respawned actor
    // rebuilds from that snapshot and only holds the events after it.
    let actor_key = format!("{}:Order:{id}", TenantId::default());
    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;
    assert!(
        store.load_snapshot(&actor_key).await.unwrap().is_some(),
        "precondition: passivation must persist a snapshot"
    );
    for n in 5..=6 {
        add_item(&state, id, n).await;
    }

    let expected = journal(&store, id).await;
    assert!(expected.len() >= 7, "journal too short: {expected:?}");

    // The record read only carries the in-memory tail: this is the gap.
    let (status, record) = get_json(&state, &format!("/tdata/Orders('{id}')")).await;
    assert_eq!(status, StatusCode::OK, "record read failed: {record:?}");
    let in_memory = record["events"].as_array().map_or(0, Vec::len);
    assert!(
        in_memory < expected.len(),
        "precondition: the in-memory list must miss events before the snapshot"
    );

    let pages = read_all_pages(&state, id, 100).await;
    assert_eq!(pages.len(), 1);
    assert_eq!(sequences_and_actions(&pages[0]), expected);
}

#[tokio::test]
async fn history_pages_by_stored_sequence() {
    let (state, store) = common::build_default_state(5002, "history-paging");
    install_policy(&state, ORDER_READ_POLICY);
    let id = "ord-history-paging";
    create_order(&state, id).await;
    for n in 1..=6 {
        add_item(&state, id, n).await;
    }
    let expected = journal(&store, id).await;

    let pages = read_all_pages(&state, id, 2).await;
    assert_eq!(pages.len(), expected.len().div_ceil(2));
    let walked: Vec<(u64, String)> = pages
        .iter()
        .flat_map(|page| sequences_and_actions(page))
        .collect();
    assert_eq!(walked, expected);

    // `after` is a stored sequence: starting after the third event returns
    // exactly the events stored after it.
    let third = expected[2].0;
    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{id}')/$history?after={third}&limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        sequences_and_actions(body["events"].as_array().unwrap()),
        expected[3..5].to_vec()
    );
    assert_eq!(body["next"].as_u64(), Some(expected[4].0));

    let last = expected.last().unwrap().0;
    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{id}')/$history?after={last}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"], serde_json::json!([]));
    assert!(body["next"].is_null());

    for bad in ["limit=0", "limit=many", "after=-1"] {
        let (status, body) =
            get_json(&state, &format!("/tdata/Orders('{id}')/$history?{bad}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body:?}");
    }
}

#[tokio::test]
async fn history_carries_each_transitions_params() {
    let (state, _store) = common::build_default_state(5003, "history-params");
    install_policy(&state, ORDER_READ_POLICY);
    let id = "ord-history-params";
    create_order(&state, id).await;
    add_item(&state, id, 1).await;
    add_item(&state, id, 2).await;

    let (status, body) = get_json(&state, &format!("/tdata/Orders('{id}')/$history")).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let moves: Vec<&serde_json::Value> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["action"] == "AddItem")
        .collect();
    assert_eq!(moves.len(), 2);
    for (n, event) in (1..=2).zip(moves) {
        assert_eq!(event["params"]["moved_via"], format!("channel-{n}"));
        assert_eq!(event["params"]["ProductId"], format!("p-{n}"));
        assert_eq!(event["params"]["Quantity"], n);
        assert_eq!(event["from_state"], "Draft");
        assert_eq!(event["to_state"], "Draft");
        assert!(event["timestamp"].is_string(), "{event:?}");
    }
}

#[tokio::test]
async fn history_is_refused_exactly_like_the_record_read() {
    let (state, _store) = common::build_default_state(5004, "history-denied");
    for id in ["ord-visible", "ord-hidden"] {
        create_order(&state, id).await;
        add_item(&state, id, 1).await;
    }

    // A caller who may list but not read gets the record read's refusal.
    install_policy(
        &state,
        r#"permit(principal, action == Action::"list", resource is Order);"#,
    );
    for id in ["ord-visible", "ord-hidden"] {
        let (record_status, record) = get_json(&state, &format!("/tdata/Orders('{id}')")).await;
        let (history_status, history) =
            get_json(&state, &format!("/tdata/Orders('{id}')/$history")).await;
        assert_eq!(record_status, StatusCode::FORBIDDEN, "{record:?}");
        assert_eq!(history_status, record_status, "{history:?}");
        assert_eq!(history["error"]["code"], record["error"]["code"]);
        assert!(history.get("events").is_none(), "{history:?}");
    }

    // A record-level forbid hides that record's history and nothing else.
    install_policy(
        &state,
        r#"
            permit(principal, action in [Action::"list", Action::"read"], resource is Order);
            forbid(principal, action == Action::"read", resource == Order::"ord-hidden");
        "#,
    );
    let (status, body) = get_json(&state, "/tdata/Orders('ord-visible')/$history").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(!body["events"].as_array().unwrap().is_empty());
    let (status, body) = get_json(&state, "/tdata/Orders('ord-hidden')/$history").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
    assert!(body.get("events").is_none(), "{body:?}");

    // An unknown record answers as the record read does.
    let (record_status, _) = get_json(&state, "/tdata/Orders('ord-missing')").await;
    let (history_status, _) = get_json(&state, "/tdata/Orders('ord-missing')/$history").await;
    assert_eq!(record_status, StatusCode::NOT_FOUND);
    assert_eq!(history_status, record_status);
}

#[tokio::test]
async fn history_of_a_deleted_record_answers_as_the_record_read() {
    let (state, _store) = common::build_default_state(5005, "history-deleted");
    install_policy(&state, ORDER_READ_POLICY);
    let id = "ord-history-deleted";
    create_order(&state, id).await;
    add_item(&state, id, 1).await;
    // The kernel's delete writes the tombstone; the OData DELETE in front of
    // it is not what this test is about.
    state
        .delete_tenant_entity(&TenantId::default(), "Order", id)
        .await
        .expect("delete order");

    let (record_status, record) = get_json(&state, &format!("/tdata/Orders('{id}')")).await;
    let (history_status, history) =
        get_json(&state, &format!("/tdata/Orders('{id}')/$history")).await;
    assert_eq!(history_status, record_status, "{record:?} / {history:?}");
    if record_status == StatusCode::OK {
        let last = history["events"].as_array().unwrap().last().unwrap();
        assert_eq!(last["action"], "Deleted");
    }
}

#[tokio::test]
async fn history_pages_a_turso_journal() {
    let db_path =
        std::env::temp_dir().join(format!("temper-record-history-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("local turso store");
    let mut registry = temper_server::registry::SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(common::CSDL_XML).expect("CSDL parse"),
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let mut state =
        ServerState::from_registry(temper_runtime::ActorSystem::new("history-turso"), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    install_policy(&state, ORDER_READ_POLICY);
    let id = "ord-history-turso";
    create_order(&state, id).await;
    for n in 1..=4 {
        add_item(&state, id, n).await;
    }
    let expected = journal(&store, id).await;
    assert!(expected.len() >= 5, "{expected:?}");

    let pages = read_all_pages(&state, id, 2).await;
    assert_eq!(pages.len(), expected.len().div_ceil(2));
    let walked: Vec<(u64, String)> = pages
        .iter()
        .flat_map(|page| sequences_and_actions(page))
        .collect();
    assert_eq!(walked, expected);
    let _ = std::fs::remove_file(&db_path);
}
