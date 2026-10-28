//! Commit visibility: a transition's query projection is written before the
//! response returns and before anything it launches can read it.
use super::effects::PostDispatchContext;
use crate::entity_actor::EntityResponse;
use crate::state::ServerState;
use std::time::Duration;
use temper_runtime::persistence::PersistenceError;

/// How long one projection write may take before it is abandoned and tried
/// again: a write stuck on a dead connection or a held lock does not finish by
/// waiting longer, and a fresh try can.
const PROJECTION_BARRIER_BUDGET: Duration = Duration::from_secs(30);

/// The wait before a failed projection write is tried again. It doubles with
/// each try, up to [`PROJECTION_BARRIER_BUDGET`], so a store that recovers is
/// written within one try's budget and a store that stays down is not hammered.
const FIRST_RETRY_WAIT: Duration = Duration::from_millis(100);

impl ServerState {
    /// One try at the actual sequence-guarded store write, not queue
    /// admission. A failure leaves the journal committed, and dependent
    /// dispatch waits for [`Self::project_until_visible`].
    pub(super) async fn project_before_dependents(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) -> Result<(), PersistenceError> {
        let Some(query_plane) = self.query_plane_written_after_commit() else {
            return Ok(());
        };
        let fields =
            self.query_projection_fields(ctx.tenant, ctx.entity_type, &response.state.fields);
        let state = self.query_projection_state(&response.state);
        // Heap allocate the store future: recursive reaction dispatch must not
        // grow every dispatch frame with another storage future.
        // Preserve the sequence guard even for Deleted. Ordered queue cleanup
        // removes that row behind older queued writes.
        tokio::time::timeout(
            PROJECTION_BARRIER_BUDGET,
            Box::pin(query_plane.upsert_projection(
                ctx.tenant.as_str(),
                ctx.entity_type,
                ctx.entity_id,
                &response.state.status,
                &fields,
                &state,
                response.state.sequence_nr,
            )),
        )
        .await
        .map_err(|_| {
            PersistenceError::Storage(format!(
                "query projection timed out after {} seconds",
                PROJECTION_BARRIER_BUDGET.as_secs()
            ))
        })?
    }

    /// Try the projection write again, after `first_error`, until it lands.
    ///
    /// A newer transition's row counts as landed: the store keeps the higher
    /// sequence and answers this write `Ok`. A delete committed since ends the
    /// tries: it removes the row rather than replacing it, so nothing guards
    /// against this older write landing after the removal and bringing the
    /// record back. The journal says whether one was committed
    /// ([`Self::deleted_since`]); a try that cannot read it writes nothing.
    /// Every failed try is logged, so a store that never takes the write is
    /// loud, not silent.
    pub(super) async fn project_until_visible(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
        first_error: PersistenceError,
    ) {
        let mut error = first_error;
        let mut wait = FIRST_RETRY_WAIT;
        let mut failed_tries: u32 = 1;
        loop {
            tracing::error!(tenant = %ctx.tenant, entity_type = ctx.entity_type,
                entity_id = ctx.entity_id, action = ctx.action, %error, failed_tries,
                retry_in_ms = wait.as_millis() as u64,
                "Committed transition projection failed; dependents wait for it");
            tokio::time::sleep(wait).await; // determinism-ok: retry backoff
            wait = (wait * 2).min(PROJECTION_BARRIER_BUDGET);
            match self.deleted_since(ctx, response).await {
                Ok(true) => {
                    tracing::warn!(tenant = %ctx.tenant, entity_type = ctx.entity_type,
                        entity_id = ctx.entity_id, action = ctx.action, failed_tries,
                        "Record deleted while its committed transition's projection failed; starting dependents without it");
                    return;
                }
                Ok(false) => {}
                Err(read_error) => {
                    error = read_error;
                    failed_tries += 1;
                    continue;
                }
            }
            match self.project_before_dependents(ctx, response).await {
                Ok(()) => break,
                Err(next) => {
                    error = next;
                    failed_tries += 1;
                }
            }
        }
        tracing::info!(tenant = %ctx.tenant, entity_type = ctx.entity_type,
            entity_id = ctx.entity_id, action = ctx.action, failed_tries,
            "Committed transition projected after failed tries; starting dependents");
    }

    /// Whether the record was deleted after `response`'s transition: whether
    /// its journal holds the kernel's tombstone past that transition's
    /// sequence. Only the tombstone removes the row, and the actor takes no
    /// action after it. A spec action into a state named `Deleted` writes its
    /// own row at a higher sequence, which an older write cannot replace. The
    /// journal is read, not the status cache: during a long outage the
    /// cache can evict the delete's status, and a retry that missed it would
    /// bring the deleted record back. An error means the journal could not be
    /// read, and the try must not write.
    async fn deleted_since(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) -> Result<bool, PersistenceError> {
        // A query plane comes with its storage stack's journal: with no
        // journal there is no projection to retry.
        let Some((store, _)) = self.event_journal() else {
            return Ok(false);
        };
        let persistence_id = format!("{}:{}:{}", ctx.tenant, ctx.entity_type, ctx.entity_id);
        let after = tokio::time::timeout(
            PROJECTION_BARRIER_BUDGET,
            Box::pin(store.read_events(&persistence_id, response.state.sequence_nr)),
        )
        .await
        .map_err(|_| {
            PersistenceError::Storage(format!(
                "journal read for a delete timed out after {} seconds",
                PROJECTION_BARRIER_BUDGET.as_secs()
            ))
        })?
        .map_err(|read_error| {
            PersistenceError::Storage(format!("journal read for a delete failed: {read_error}"))
        })?;
        Ok(after
            .iter()
            .any(temper_runtime::persistence::PersistenceEnvelope::is_tombstone))
    }
}
