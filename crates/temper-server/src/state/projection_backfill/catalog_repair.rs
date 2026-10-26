//! Boot repair for catalog rows their journal is ahead of.
//!
//! A record's catalog row is written in its own commit after its event, so a
//! process that stops between the two leaves events the catalog has not
//! projected: no row at all, or a row from an older event. Reads served by
//! the catalog then miss the record or show it as it was. At boot the store
//! lists only those records, and each one is replayed from its journal and
//! projected again. Current rows are neither replayed nor rewritten, so a
//! large tenant on a slow drive pays for its drift, not for its size, and the
//! repair holds the write lock only for the rows it fixes.

use std::sync::Arc;

use temper_runtime::tenant::TenantId;

use crate::entity_actor::recover_entity_state_from_store;
use crate::storage::{BackendLabel, BoxedEventStore, JournalAheadOfCatalog, QueryPlaneStore};

use super::{ServerState, transition_table_for};

/// The stores one repair reads and writes.
struct RepairStores {
    query_plane: Arc<dyn QueryPlaneStore>,
    events: BoxedEventStore,
    backend: BackendLabel,
}

/// What the repair did in one tenant, for its summary line.
#[derive(Default)]
struct TenantRepairSummary {
    /// Entity types whose drifted records the store listed.
    checked_types: usize,
    /// Records whose catalog row was written or removed.
    repaired: usize,
    /// Records the repair could not bring up to the journal.
    failed: usize,
}

impl ServerState {
    /// Bring every catalog row the journal is ahead of up to the journal, in
    /// every tenant the registry holds and every entity type each one holds.
    ///
    /// Meant to run once at boot after installed apps are restored, so their
    /// types are registered. Only the records the store reports as behind are
    /// replayed. A record whose replay ends `Deleted` has its row removed.
    /// Logs one summary per tenant, and a warning per record it could not
    /// repair. A store that cannot compare its journal with its catalog is
    /// logged once, by backend, and nothing is repaired.
    pub async fn repair_catalog_rows_behind_journal(&self) {
        let Some(query_plane) = self.query_plane_store() else {
            return;
        };
        let Some((events, backend)) = self.event_journal() else {
            return;
        };
        let stores = RepairStores {
            query_plane,
            events,
            backend,
        };
        let tenants: Vec<TenantId> = {
            let registry = self.registry.read().unwrap();
            registry.tenant_ids().into_iter().cloned().collect()
        };

        for tenant in &tenants {
            let entity_types: Vec<String> = {
                let registry = self.registry.read().unwrap();
                registry
                    .entity_types(tenant)
                    .into_iter()
                    .map(ToString::to_string)
                    .collect()
            };
            let mut summary = TenantRepairSummary::default();
            for entity_type in &entity_types {
                let ids = match stores
                    .query_plane
                    .entity_ids_ahead_of_catalog(tenant.as_str(), entity_type)
                    .await
                {
                    Ok(JournalAheadOfCatalog::Ids(ids)) => ids,
                    Ok(JournalAheadOfCatalog::Unsupported) => {
                        tracing::info!(
                            backend = stores.backend.as_str(),
                            "catalog repair: this store cannot compare its journal with its catalog; catalog rows behind the journal are not repaired at boot"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            tenant = %tenant,
                            entity_type = %entity_type,
                            error = %error,
                            "catalog repair: failed to list records the catalog is behind; type skipped"
                        );
                        continue;
                    }
                };
                summary.checked_types += 1;
                for entity_id in &ids {
                    tokio::task::yield_now().await;
                    match self
                        .repair_catalog_row(&stores, tenant, entity_type, entity_id)
                        .await
                    {
                        Ok(()) => summary.repaired += 1,
                        Err(error) => {
                            summary.failed += 1;
                            tracing::warn!(
                                tenant = %tenant,
                                entity_type = %entity_type,
                                entity_id = %entity_id,
                                error = %error,
                                "catalog repair: could not bring the catalog row up to the journal"
                            );
                        }
                    }
                }
            }
            tracing::info!(
                tenant = %tenant,
                checked_types = summary.checked_types,
                repaired = summary.repaired,
                failed = summary.failed,
                "catalog repair: brought catalog rows up to the journal"
            );
        }
    }

    /// Replay one record from its journal and write, or remove, its catalog row.
    async fn repair_catalog_row(
        &self,
        stores: &RepairStores,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), String> {
        let table = transition_table_for(self, tenant, entity_type)
            .ok_or_else(|| "no transition table to replay with".to_string())?;
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        let replayed = recover_entity_state_from_store(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &stores.events,
            stores.backend,
            &serde_json::json!({}),
            blob_store.as_ref(),
        )
        .await
        .map_err(|error| format!("replay failed: {error}"))?;
        if replayed.total_event_count == 0 {
            return Err("replay found no events".to_string());
        }
        if replayed.status == "Deleted" {
            return stores
                .query_plane
                .remove_projection(tenant.as_str(), entity_type, entity_id)
                .await
                .map_err(|error| format!("removing the catalog row failed: {error}"));
        }
        stores
            .query_plane
            .upsert_projection(
                tenant.as_str(),
                entity_type,
                entity_id,
                &replayed.status,
                &self.query_projection_fields(tenant, entity_type, &replayed.fields),
                &self.query_projection_state(&replayed),
                replayed.sequence_nr,
            )
            .await
            .map_err(|error| format!("writing the catalog row failed: {error}"))
    }
}
