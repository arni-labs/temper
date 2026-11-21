//! A spawned child's initial action as one try sends it, checked first.

use super::ChildSpawn;
use crate::blobs::StoredReadError;
use crate::state::ServerState;
use crate::state::dispatch::owed_writes::Attempt;

impl ServerState {
    /// `child`'s initial action as this try sends it, checked against the
    /// child it would be sent to.
    ///
    /// A copied parent field the parent stores out of line is sent as its
    /// value (`blobs::resolve_copied_values`), read in this try so a store
    /// that does not answer keeps the child for another. One that can never
    /// be read stays a reference, which the check refuses. Existing actors
    /// validate against their hydrated blob values and execution-time state;
    /// only an absent child needs this check before creation can persist
    /// anything. A read of the child the store failed is tried again with the
    /// spawn; a refused check is final.
    pub(super) async fn child_initializer(
        &self,
        child: &ChildSpawn,
    ) -> Result<Option<(String, serde_json::Value)>, Attempt> {
        let Some((action, copied)) = &child.initializer else {
            return Ok(None);
        };
        let (tenant, child_type, child_id) = (&child.tenant, &child.child_type, &child.child_id);
        let params = match self.copied_values(tenant, copied).await {
            Ok(params) => params,
            Err(StoredReadError::Unavailable(error)) => return Err(Attempt::StoreFailed(error)),
            Err(StoredReadError::Invalid(_)) => copied.clone(),
        };
        let refused = match self
            .load_authz_resource_snapshot(tenant, child_type, child_id)
            .await
        {
            Ok(snapshot) if snapshot.exists => return Ok(Some((action.clone(), params))),
            Ok(_) => {
                let table = &child.table;
                let prestate = crate::entity_actor::EntityActor::build_initial_state(
                    child_type,
                    child_id,
                    table,
                    &child.initial_fields,
                );
                match table.validate_action_params(
                    action,
                    &params,
                    &prestate.fields,
                    &prestate.counters,
                    &prestate.booleans,
                ) {
                    Ok(()) => return Ok(Some((action.clone(), params))),
                    Err(error) => String::from(error),
                }
            }
            Err(error) => match Attempt::of_error(&error) {
                Attempt::Failed(refused) => refused,
                attempt => return Err(attempt),
            },
        };
        self.refuse_spawn(
            tenant,
            &child.parent_type,
            &child.parent_id,
            &child.request,
            action,
            &refused,
        )
        .await;
        Err(Attempt::Failed(refused))
    }
}
