//! Commit visibility: a transition's query projection is written before the
//! response returns and before anything it launches can read it.
use super::effects::PostDispatchContext;
use crate::entity_actor::EntityResponse;
use crate::state::ServerState;
use std::time::Duration;
use temper_runtime::persistence::PersistenceError;

const PROJECTION_BARRIER_BUDGET: Duration = Duration::from_secs(30);

impl ServerState {
    /// Await the actual sequence-guarded store write, not queue admission.
    /// Failure leaves the journal committed, is reported in the response, and
    /// must prevent dependent dispatch.
    pub(super) async fn project_before_dependents(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) -> Result<(), PersistenceError> {
        let Some(query_plane) = self.query_plane_store() else {
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
}
