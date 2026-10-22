//! Erasing a deleted entity's history: the state work around the store's transaction.

use serde_json::json;
use temper_runtime::persistence::{HistoryErase, HistoryErased};
use temper_runtime::tenant::TenantId;

use super::ServerState;
use crate::storage::{HistoryEraseStore as _, MetadataStore};

impl ServerState {
    /// Erase one deleted entity's history (see `HistoryErase`); refuses a live one.
    ///
    /// # Errors
    ///
    /// Returns why nothing was erased: the entity is not deleted, or the store refused.
    pub async fn erase_entity_history(
        &self,
        tenant: &TenantId,
        store: &dyn MetadataStore,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<HistoryErased, String> {
        let state = self;
        let current = state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await?
            .state;
        let tombstone = current
            .events
            .back()
            .filter(|event| current.status == "Deleted" && event.action == "Deleted")
            .ok_or_else(|| "only a deleted entity's history can be erased".to_string())?;

        // The tombstone keeps when and from which status, never what was sent.
        let mut kept = tombstone.clone();
        kept.params = json!({});
        kept.idempotency_key = None;
        let tombstone_payload = serde_json::to_string(&kept).map_err(|e| e.to_string())?;

        // The state just before the tombstone, with nothing but the id.
        let before = current.sequence_nr.saturating_sub(1);
        let mut snapshot = serde_json::to_value(&current).map_err(|e| e.to_string())?;
        if let Some(obj) = snapshot.as_object_mut() {
            obj.insert("status".into(), json!(kept.from_status));
            obj.insert("fields".into(), json!({ "Id": entity_id }));
            for empty in [
                "counters",
                "booleans",
                "lists",
                "processed_idempotency_keys",
            ] {
                obj.insert(empty.into(), json!({}));
            }
            obj.insert("events".into(), json!([]));
            obj.insert("item_count".into(), json!(0));
            obj.insert("status_clock".into(), json!({}));
            for seq in [
                "sequence_nr",
                "total_event_count",
                "last_snapshot_sequence_nr",
            ] {
                obj.insert(seq.into(), json!(before));
            }
            obj.insert("events_since_snapshot".into(), json!(0));
        }
        let snapshot = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;

        let erased = store
            .erase_entity_history(HistoryErase {
                tenant: tenant.as_str(),
                entity_type,
                entity_id,
                tombstone_sequence_nr: current.sequence_nr,
                tombstone_payload: &tombstone_payload,
                snapshot: &snapshot,
            })
            .await
            .map_err(|e| e.to_string())?;

        // The running actor still holds the old history in memory: stop it, so the
        // next read replays what is left.
        if let Some(actor) = state.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) {
            let _ = actor.stop();
        }
        state.remove_entity(tenant, entity_type, entity_id);
        if let Some(query_plane) = state.query_plane_store()
            && let Err(error) = query_plane
                .remove_projection(tenant.as_str(), entity_type, entity_id)
                .await
        {
            tracing::warn!(%error, entity_type, entity_id, "erase left a projection row");
        }
        Ok(erased)
    }
}
