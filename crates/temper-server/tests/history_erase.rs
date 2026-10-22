//! Erasing a deleted entity's history leaves no trace of what it held, and the
//! entity still replays as deleted at the same sequence.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::{TursoEventStore, TursoTrajectoryInsert};

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const SECRET: &str = "erase-me-7f3c9a";

async fn state_on(db_path: &std::path::Path) -> (ServerState, TursoEventStore) {
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("open turso file");
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("history-erase"), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store)
}

/// Whether any byte of the database file or its log holds `needle`.
fn file_holds(db_path: &std::path::Path, needle: &str) -> bool {
    ["", "-wal"].iter().any(|suffix| {
        let path = format!("{}{suffix}", db_path.display());
        std::fs::read(&path)
            .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
            .unwrap_or(false)
    })
}

#[tokio::test]
async fn an_erased_entity_keeps_its_tombstone_and_nothing_it_held() {
    let db_path = std::env::temp_dir().join(format!("temper-erase-{}.db", uuid::Uuid::new_v4()));
    let (state, store) = state_on(&db_path).await;
    let tenant = TenantId::new("tenant-a");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "ord-1",
            serde_json::json!({ "Note": SECRET }),
        )
        .await
        .expect("create");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-1",
            "AddItem",
            serde_json::json!({ "ProductId": SECRET, "Quantity": 1 }),
            &temper_server::request_context::AgentContext::system(),
        )
        .await
        .expect("add item");
    store
        .persist_trajectory(TursoTrajectoryInsert {
            tenant: "tenant-a",
            entity_type: "Order",
            entity_id: "ord-1",
            action: "AddItem",
            success: true,
            from_status: Some("Draft"),
            to_status: Some("Draft"),
            error: None,
            agent_id: None,
            session_id: None,
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: None,
            created_at: "2026-10-22T00:00:00Z",
            request_body: Some(&format!(r#"{{"ProductId":"{SECRET}"}}"#)),
            intent: None,
            matched_policy_ids: None,
            capture_seq: None,
        })
        .await
        .expect("trajectory");
    let store_ref: &dyn temper_server::storage::MetadataStore = &store;

    let live = state
        .erase_entity_history(&tenant, store_ref, "Order", "ord-1")
        .await;
    assert!(live.is_err(), "a live entity is refused");

    let deleted = state
        .delete_tenant_entity(&tenant, "Order", "ord-1")
        .await
        .expect("delete");
    let tombstone_seq = deleted.state.sequence_nr;
    assert!(
        file_holds(&db_path, SECRET),
        "the journal held the value before"
    );

    let erased = state
        .erase_entity_history(&tenant, store_ref, "Order", "ord-1")
        .await
        .expect("erase");
    assert_eq!(erased.events, tombstone_seq - 1, "{erased:?}");
    assert!(erased.trajectories >= 1, "{erased:?}");

    let replayed = state
        .get_tenant_entity_state(&tenant, "Order", "ord-1")
        .await
        .expect("replay")
        .state;
    assert_eq!(replayed.status, "Deleted");
    assert_eq!(replayed.sequence_nr, tombstone_seq);
    assert!(!replayed.fields.to_string().contains(SECRET));
    assert!(
        !file_holds(&db_path, SECRET),
        "the value survived in the file"
    );

    let again = state
        .erase_entity_history(&tenant, store_ref, "Order", "ord-1")
        .await
        .expect("erasing again is a no-op");
    assert_eq!(again.events, 0);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
    }
}
