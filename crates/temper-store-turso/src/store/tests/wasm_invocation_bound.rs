//! A tenant's WASM invocation log keeps its newest rows, up to the bound its
//! only reader reads, and pruning one tenant leaves the others alone.

use temper_runtime::persistence::{WASM_INVOCATIONS_KEPT_PER_TENANT, WASM_INVOCATIONS_PRUNE_EVERY};

use super::make_store;
use crate::TursoWasmInvocationInsert;

fn invocation<'a>(tenant: &'a str, entity_id: &'a str) -> TursoWasmInvocationInsert<'a> {
    TursoWasmInvocationInsert {
        tenant,
        entity_type: "Job",
        entity_id,
        module_name: "poll",
        trigger_action: "Poll",
        callback_action: None,
        success: true,
        error: None,
        duration_ms: 1,
        created_at: "2026-11-02T00:00:00Z",
    }
}

#[tokio::test]
async fn a_tenants_invocation_log_keeps_its_newest_rows_up_to_the_bound() {
    let store = make_store("wasm-invocation-bound").await;
    for id in ["q-0", "q-1", "q-2"] {
        store
            .persist_wasm_invocation(&invocation("quiet", id))
            .await
            .unwrap();
    }
    let busy = WASM_INVOCATIONS_KEPT_PER_TENANT + 2 * WASM_INVOCATIONS_PRUNE_EVERY + 50;
    for n in 0..busy {
        let id = format!("e-{n}");
        store
            .persist_wasm_invocation(&invocation("busy", &id))
            .await
            .unwrap();
    }
    let rows = store.load_recent_wasm_invocations(2 * busy).await.unwrap();
    let kept: Vec<&str> = rows
        .iter()
        .filter(|row| row.tenant == "busy")
        .map(|row| row.entity_id.as_str())
        .collect();
    let kept_count = kept.len() as i64;
    assert!(
        (WASM_INVOCATIONS_KEPT_PER_TENANT
            ..WASM_INVOCATIONS_KEPT_PER_TENANT + WASM_INVOCATIONS_PRUNE_EVERY)
            .contains(&kept_count),
        "the busy tenant keeps the bound and at most one prune's worth more: {kept_count}"
    );
    let newest = format!("e-{}", busy - 1);
    assert!(kept.contains(&newest.as_str()), "its newest row is kept");
    assert!(!kept.contains(&"e-0"), "its oldest rows are pruned");
    assert_eq!(
        rows.iter().filter(|row| row.tenant == "quiet").count(),
        3,
        "another tenant's rows are left alone"
    );
}
