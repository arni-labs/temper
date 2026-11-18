//! ADR-0186 meaning backfill: give a meaning row to every record saved before its
//! store kept them, or before the current text rule ([`TEXT_RULE`]), and mark each
//! type done.
//!
//! The same enumeration and state load as the key backfill (`key_index.rs`): the
//! registry's types, the store's ids, each record's current state. A type is marked
//! only when every record has its row or definitively has none.

use temper_runtime::persistence::{TEXT_RULE, record_text};
use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

/// Write the meaning row of every existing record of every type of `tenant` not yet
/// marked under [`TEXT_RULE`], then mark the type.
///
/// Idempotent: a row whose text is unchanged keeps its vector. Records saved after
/// the store kept meaning rows already have theirs. Runs off the boot path.
pub(in crate::state) async fn populate_meaning_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
    let Some((store, backend)) = state.event_journal() else {
        return;
    };
    let entity_types: Vec<String> = {
        let registry = state.registry.read().expect("registry lock poisoned");
        registry
            .entity_types(tenant)
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    let marked: std::collections::BTreeMap<String, String> = store
        .meaning_backfilled_types(tenant.as_str())
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    for entity_type in &entity_types {
        if marked.get(entity_type).map(String::as_str) == Some(TEXT_RULE) {
            continue;
        }
        let entity_ids = match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "meaning backfill: could not list records; type not marked"
                );
                continue;
            }
        };
        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let (mut written, mut removed, mut failed) = (0usize, 0usize, 0usize);

        for entity_id in &entity_ids {
            let text = match load_entity_current_fields(
                tenant,
                entity_type,
                entity_id,
                table.as_ref(),
                &store,
                backend,
                blob_store.as_ref(),
            )
            .await
            {
                EntityLoadOutcome::Fields(fields) => Some(record_text(
                    &state.query_projection_fields(tenant, entity_type, &fields),
                )),
                // Deleted or never created: it has no meaning row.
                EntityLoadOutcome::Skip => None,
                EntityLoadOutcome::LoadFailed => {
                    failed += 1;
                    tracing::warn!(
                        entity_type = %entity_type, entity_id = %entity_id,
                        "meaning backfill: record could not be loaded; type will not be marked"
                    );
                    continue;
                }
            };
            let is_text = text.is_some();
            match store
                .write_meaning(tenant.as_str(), entity_type, entity_id, text.as_deref())
                .await
            {
                Ok(()) if is_text => written += 1,
                Ok(()) => removed += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        error = %e, entity_type = %entity_type, entity_id = %entity_id,
                        "meaning backfill: write failed"
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        if failed > 0 {
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type, written, removed, failed,
                "meaning backfill: {failed} records unresolved; type not marked (the next run resumes)"
            );
            continue;
        }
        if let Err(e) = store
            .mark_meaning_backfilled(tenant.as_str(), entity_type, TEXT_RULE)
            .await
        {
            tracing::error!(
                tenant = %tenant, entity_type = %entity_type, error = %e,
                "meaning backfill: could not mark the type"
            );
            continue;
        }
        tracing::info!(
            tenant = %tenant, entity_type = %entity_type, rule = TEXT_RULE, written, removed,
            "meaning backfill complete; type marked"
        );
    }
}
