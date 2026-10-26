//! A restarted server fills in catalog rows its journal is ahead of.
//!
//! A record's catalog row is written in its own commit after its event, so a
//! process that stops between the two leaves the record's events with no
//! catalog row, or a row older than its journal. The boot repair must bring
//! those rows up to the journal and leave every current row alone.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const TENANT: &str = "tenant-a";
const ENTITY_TYPE: &str = "Order";

fn build_state(system_name: &str, store: &TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        TENANT,
        csdl,
        CSDL_XML.to_string(),
        &[(ENTITY_TYPE, ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
}

/// One catalog row as `(entity_id, Title, sequence_nr)`.
type CatalogRow = (String, Option<String>, u64);

async fn catalog_rows(store: &TursoEventStore) -> Vec<CatalogRow> {
    store
        .export_query_projections(Some(TENANT))
        .await
        .expect("export catalog rows")
        .into_iter()
        .filter(|row| row.entity_type == ENTITY_TYPE)
        .map(|row| {
            let title = row
                .fields
                .get("Title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            (row.entity_id, title, row.sequence_nr)
        })
        .collect()
}

/// Wait for the writer's queued projections to land, so nothing it queued
/// can arrive after the test edits the catalog.
async fn wait_for_catalog_rows(store: &TursoEventStore, expected: &[CatalogRow]) {
    let mut last = Vec::new();
    for _ in 0..100 {
        last = catalog_rows(store).await;
        if last == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("writer projections did not land: expected {expected:?}, got {last:?}");
}

async fn write_catalog_row(store: &TursoEventStore, entity_id: &str, title: &str, seq: u64) {
    let fields = serde_json::json!({ "Id": entity_id, "Title": title });
    let state = serde_json::json!({
        "entity_type": ENTITY_TYPE,
        "entity_id": entity_id,
        "status": "Draft",
        "fields": fields,
        "events": [],
        "sequence_nr": seq,
    });
    store
        .upsert_query_projection_with_state(
            TENANT,
            ENTITY_TYPE,
            entity_id,
            "Draft",
            &fields,
            &state,
            seq,
        )
        .await
        .expect("write catalog row");
}

#[tokio::test]
async fn boot_repair_fills_catalog_rows_the_journal_is_ahead_of() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-catalog-boot-repair-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::new(TENANT);

    // The process before the restart: three records, each fully projected.
    let writer = build_state("catalog-boot-repair-writer", &store);
    let mut seqs = Vec::new();
    for (entity_id, title) in [
        ("ord-current", "Current"),
        ("ord-missing", "Missing"),
        ("ord-behind", "Behind"),
    ] {
        let created = writer
            .get_or_create_tenant_entity(
                &tenant,
                ENTITY_TYPE,
                entity_id,
                serde_json::json!({ "Title": title }),
            )
            .await
            .expect("create order");
        seqs.push(created.state.sequence_nr);
    }
    let added = writer
        .dispatch_tenant_action(
            &tenant,
            ENTITY_TYPE,
            "ord-behind",
            "AddItem",
            serde_json::json!({ "ProductId": "sku-1", "Quantity": 1 }),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch AddItem");
    assert!(added.success, "AddItem should succeed");
    let behind_seq = added.state.sequence_nr;
    assert!(behind_seq > seqs[2], "AddItem should journal a newer event");
    wait_for_catalog_rows(
        &store,
        &[
            (
                "ord-behind".to_string(),
                Some("Behind".to_string()),
                behind_seq,
            ),
            (
                "ord-current".to_string(),
                Some("Current".to_string()),
                seqs[0],
            ),
            (
                "ord-missing".to_string(),
                Some("Missing".to_string()),
                seqs[1],
            ),
        ],
    )
    .await;

    // What a stop between the event commit and the catalog commit leaves:
    // `ord-missing` has events and no catalog row, and `ord-behind`'s row is
    // from before its last event.
    store
        .remove_query_projection(TENANT, ENTITY_TYPE, "ord-missing")
        .await
        .expect("remove catalog row");
    store
        .remove_query_projection(TENANT, ENTITY_TYPE, "ord-behind")
        .await
        .expect("remove catalog row");
    write_catalog_row(&store, "ord-behind", "Behind (old)", seqs[2]).await;
    // `ord-current`'s row is at its journal's sequence. Its title marks it, so
    // a repair that rewrote every row would show here.
    write_catalog_row(&store, "ord-current", "Untouched", seqs[0]).await;

    // The restarted process.
    let restarted = build_state("catalog-boot-repair-restarted", &store);
    restarted.repair_catalog_rows_behind_journal().await;

    assert_eq!(
        catalog_rows(&store).await,
        vec![
            (
                "ord-behind".to_string(),
                Some("Behind".to_string()),
                behind_seq
            ),
            (
                "ord-current".to_string(),
                Some("Untouched".to_string()),
                seqs[0]
            ),
            (
                "ord-missing".to_string(),
                Some("Missing".to_string()),
                seqs[1]
            ),
        ],
        "the repair should bring rows behind the journal up to it and leave the current row alone"
    );

    let _ = std::fs::remove_file(db_path);
}
