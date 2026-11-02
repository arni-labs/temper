//! Persist the reason a timer cannot name its caller.

use super::{EntityMsg, EntityResponse, ServerState, TenantId};

impl ServerState {
    /// Record on a record that its state timer stopped because nothing names
    /// its caller. See `entity_actor::timer_needs_person`.
    pub(crate) async fn record_timer_needs_person(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        checked: u64,
    ) -> Result<crate::entity_actor::TimerStop, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;
        let response: EntityResponse = actor_ref
            .ask(
                EntityMsg::RecordTimerNeedsPerson {
                    action: action.to_owned(),
                    checked,
                },
                self.action_dispatch_timeout,
            )
            .await
            .map_err(|e| format!("Actor update failed: {e}"))?;
        if response.refused && response.error.is_none() {
            return Ok(crate::entity_actor::TimerStop::Moved);
        }
        if !response.success {
            return Err(response
                .error
                .unwrap_or_else(|| "the stopped timer was not recorded".to_owned()));
        }
        self.upsert_kernel_write_projection(
            tenant,
            entity_type,
            entity_id,
            &response,
            "timer_needs_person",
        )
        .await?;
        Ok(crate::entity_actor::TimerStop::Said)
    }
}
