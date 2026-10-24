//! Reading an entity that must already exist, without ever starting one.

use temper_runtime::actor::ActorRef;
use temper_runtime::tenant::TenantId;

use super::ServerState;
use super::dispatch::retry;
use crate::entity_actor::{EntityMsg, EntityResponse};

impl ServerState {
    /// The current state of an entity that exists, or `None` when it does
    /// not exist or was deleted.
    ///
    /// Never starts an entity for an unknown or deleted id. A live actor is
    /// asked directly; otherwise the entity is loaded only through
    /// [`Self::load_entity_from_store`], which reads the journal first and
    /// stops an actor that recovers the deletion tombstone. The actor is then
    /// looked up again, not spawned, so a delete that finishes in between
    /// leaves nothing behind. `Err` means existence is unknown (a store or
    /// actor failure).
    pub(crate) async fn get_existing_entity_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<EntityResponse>, String> {
        if self.live_actor(tenant, entity_type, entity_id).is_none()
            && !self
                .load_entity_from_store(tenant, entity_type, entity_id)
                .await?
        {
            return Ok(None);
        }
        let Some(actor_ref) = self.live_actor(tenant, entity_type, entity_id) else {
            return Ok(None);
        };
        let policy = self.dispatch_retry_policy();
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::GetState,
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor query failed: {e}"))?;
        Ok((response.state.status != "Deleted").then_some(response))
    }

    fn live_actor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let registry = self
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        registry
            .get(&key)
            .filter(|actor| !actor.is_closed())
            .cloned()
    }
}
