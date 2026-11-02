use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use crate::state::ServerState;
use crate::storage::QueryProjectionUpsert;

use super::{AtomicCompositeStream, DispatchError};

impl ServerState {
    pub(super) async fn update_composite_query_projections(
        &self,
        tenant: &TenantId,
        streams: &BTreeMap<String, AtomicCompositeStream>,
    ) -> Result<(), DispatchError> {
        // A composite batch appends its streams without their rows, so their
        // projections are written here, before the batch answers.
        let query_plane = self.query_plane_store();
        let mut projection_upserts = Vec::new();

        for stream in streams.values().filter(|stream| !stream.events.is_empty()) {
            self.cache_entity_status(tenant, &stream.state);
            self.index_composite_stream(tenant, stream)?;
            self.clear_commons_storage_projection_cache_for_entity(&stream.entity_type);

            if query_plane.is_some() {
                projection_upserts.push(self.composite_projection(tenant, stream));
            }
        }

        if let Some(query_plane) = query_plane
            && !projection_upserts.is_empty()
        {
            query_plane
                .upsert_projections(tenant.as_str(), &projection_upserts)
                .await
                .map_err(|e| {
                    DispatchError::Internal(format!(
                        "query projection write failed after composite batch: {e}"
                    ))
                })?;
        }

        Ok(())
    }

    fn index_composite_stream(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
    ) -> Result<(), DispatchError> {
        let index_key = format!("{tenant}:{}", stream.entity_type);
        let mut index = self.entity_index.write().map_err(|_| {
            DispatchError::Internal("entity index lock poisoned after composite batch".to_string())
        })?;
        index
            .entry(index_key)
            .or_default()
            .insert(stream.entity_id.clone());
        Ok(())
    }

    fn composite_projection(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
    ) -> QueryProjectionUpsert {
        let fields = stream.state.fields.clone();
        let projected_state = self.query_projection_state(&stream.state);
        let indexed_fields = self.query_projection_fields(tenant, &stream.entity_type, &fields);
        QueryProjectionUpsert {
            entity_type: stream.entity_type.clone(),
            entity_id: stream.entity_id.clone(),
            status: stream.state.status.clone(),
            fields,
            state: projected_state,
            indexed_fields,
            sequence_nr: stream.state.sequence_nr,
            known_new: !stream.target_existed,
        }
    }
}
