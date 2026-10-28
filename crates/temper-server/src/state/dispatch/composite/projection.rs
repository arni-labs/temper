use std::collections::BTreeMap;
use std::time::Duration;

use temper_runtime::persistence::AppendProjection;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;
use crate::storage::{QueryPlaneStore, QueryProjectionUpsert};

use super::super::projection_barrier::{FIRST_RETRY_WAIT, PROJECTION_BARRIER_BUDGET};
use super::{AtomicCompositeStream, DispatchError};

impl ServerState {
    /// The stream's query-plane row for its batch append to commit, when the
    /// store writes rows in its commit ([`Self::entity_rows_follow_commits`]).
    /// A batch's events are spec actions, never the kernel's tombstone, so
    /// the row is kept, as a single move's is.
    pub(super) fn committed_batch_projection(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
    ) -> Option<AppendProjection> {
        if !self.entity_rows_follow_commits() {
            return None;
        }
        Some(AppendProjection::Upsert {
            status: stream.state.status.clone(),
            fields: self.query_projection_fields(tenant, &stream.entity_type, &stream.state.fields),
            state: self.query_projection_state(&stream.state),
        })
    }

    /// After a batch commits: the status cache, the entity index, and the
    /// query rows its commit did not write, which are written before the
    /// batch answers, as a single move's are (`project_before_dependents`).
    pub(super) async fn update_composite_query_projections(
        &self,
        tenant: &TenantId,
        streams: &BTreeMap<String, AtomicCompositeStream>,
    ) -> Result<(), DispatchError> {
        let query_plane = self.query_plane_written_after_commit();
        let mut upserts = Vec::new();

        for stream in streams.values().filter(|stream| !stream.events.is_empty()) {
            self.cache_entity_status(tenant, &stream.state);
            self.index_composite_stream(tenant, stream)?;
            self.clear_commons_storage_projection_cache_for_entity(&stream.entity_type);
            if query_plane.is_some() {
                upserts.push(self.batch_upsert(tenant, stream));
            }
        }

        if let Some(query_plane) = query_plane
            && !upserts.is_empty()
        {
            self.project_batch_until_visible(tenant, query_plane.as_ref(), upserts)
                .await;
        }
        Ok(())
    }

    fn batch_upsert(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
    ) -> QueryProjectionUpsert {
        let fields = stream.state.fields.clone();
        QueryProjectionUpsert {
            entity_type: stream.entity_type.clone(),
            entity_id: stream.entity_id.clone(),
            status: stream.state.status.clone(),
            indexed_fields: self.query_projection_fields(tenant, &stream.entity_type, &fields),
            state: self.query_projection_state(&stream.state),
            fields,
            sequence_nr: stream.state.sequence_nr,
            known_new: !stream.target_existed,
        }
    }

    /// Write a committed batch's rows, trying again until they land or
    /// [`PROJECTION_BARRIER_BUDGET`] is spent on tries and waits. Each row is
    /// sequence-guarded, so a newer move's row stays. A record the kernel has
    /// deleted since is left out of the next try: its row was removed, and
    /// this older write would bring it back.
    ///
    /// Past the budget the batch still answers as committed: it is, and an
    /// error would invite a retry that applies it twice. Each row left
    /// unwritten is logged and counted (`batch_unprojected`) instead.
    async fn project_batch_until_visible(
        &self,
        tenant: &TenantId,
        query_plane: &dyn QueryPlaneStore,
        mut upserts: Vec<QueryProjectionUpsert>,
    ) {
        let mut spent = Duration::ZERO;
        let mut wait = FIRST_RETRY_WAIT;
        let mut tries: u32 = 0;
        loop {
            debug_assert!(!upserts.is_empty());
            tries += 1;
            let budget = PROJECTION_BARRIER_BUDGET.saturating_sub(spent);
            let written = tokio::time::timeout(
                budget,
                Box::pin(query_plane.upsert_projections(tenant.as_str(), &upserts)),
            )
            .await;
            let mut error = match written {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(_) => {
                    spent = PROJECTION_BARRIER_BUDGET;
                    format!("timed out after {} ms", budget.as_millis())
                }
            };
            // Wait, then drop the records deleted since, until a try may run.
            loop {
                if spent.saturating_add(wait) >= PROJECTION_BARRIER_BUDGET {
                    report_unprojected(tenant, &upserts, tries, &error);
                    return;
                }
                tracing::error!(tenant = %tenant, rows = upserts.len(), tries, %error,
                    retry_in_ms = wait.as_millis() as u64,
                    "Committed composite batch projection failed; the batch waits for it");
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                spent += wait;
                wait = (wait * 2).min(PROJECTION_BARRIER_BUDGET);
                match self.without_tombstoned(tenant, &upserts).await {
                    Ok(kept) => {
                        upserts = kept;
                        break;
                    }
                    Err(read_error) => error = read_error,
                }
            }
            if upserts.is_empty() {
                return;
            }
        }
    }

    /// `upserts` without the records whose journal holds the kernel's
    /// tombstone after the row's sequence. An error means a journal could not
    /// be read, and the try must not write.
    async fn without_tombstoned(
        &self,
        tenant: &TenantId,
        upserts: &[QueryProjectionUpsert],
    ) -> Result<Vec<QueryProjectionUpsert>, String> {
        let mut kept = Vec::with_capacity(upserts.len());
        for row in upserts {
            let deleted = self
                .tombstoned_after(tenant, &row.entity_type, &row.entity_id, row.sequence_nr)
                .await
                .map_err(|error| error.to_string())?;
            if !deleted {
                kept.push(row.clone());
            }
        }
        Ok(kept)
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
}

/// The rows a committed batch answered without: loud, and counted, so a
/// query plane that stays down pages rather than sits in the logs.
fn report_unprojected(
    tenant: &TenantId,
    upserts: &[QueryProjectionUpsert],
    tries: u32,
    last_error: &str,
) {
    for row in upserts {
        tracing::error!(tenant = %tenant, entity_type = %row.entity_type,
            entity_id = %row.entity_id, sequence_nr = row.sequence_nr, tries, last_error,
            "Committed composite batch answered before its query row was written");
        crate::query_projection_metrics::record_update_error(
            tenant.as_str(),
            &row.entity_type,
            "upsert",
            "batch_unprojected",
        );
    }
}
