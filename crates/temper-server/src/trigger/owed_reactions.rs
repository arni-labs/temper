//! A reaction's move on its target, owed once the source move commits.
//!
//! The move goes through the core dispatch, without its own reaction cascade:
//! [`super::ReactionDispatcher`] runs the cascade of a move that lands at once.
//! A move the store failed is kept (`state::dispatch::owed_writes`) and sent
//! again under the reaction's key until it lands or is refused, and its
//! cascade runs when it lands.

use std::sync::{Arc, PoisonError};

use temper_runtime::tenant::TenantId;

use super::types::ReactionTarget;
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::owed_writes::{Attempt, OwedKind, OwedMove, OwedTry};
use crate::state::{DispatchError, ServerState};

impl ServerState {
    /// Send a reaction at cascade `depth`: `target`'s action on `target_id`
    /// with `params`, as `ctx`, which carries the reaction's key.
    pub(crate) async fn dispatch_reaction(
        &self,
        tenant: &TenantId,
        target: &ReactionTarget,
        target_id: &str,
        params: serde_json::Value,
        ctx: &AgentContext,
        depth: u32,
    ) -> Result<EntityResponse, DispatchError> {
        let sent = OwedMove {
            tenant: tenant.clone(),
            entity_type: target.entity_type.clone(),
            entity_id: target_id.to_string(),
            action: target.action.clone(),
            params,
            agent_ctx: ctx.clone(),
        };
        let reply = send_reaction(self, &sent).await;
        if let Attempt::StoreFailed(error) = Attempt::of(&reply) {
            let owed = sent.owed(OwedKind::Reaction);
            let sent = Arc::new(sent);
            self.keep_owed_write(owed, &error, move |state| {
                react_again(state, Arc::clone(&sent), depth)
            });
        }
        reply
    }
}

async fn send_reaction(
    state: &ServerState,
    sent: &OwedMove,
) -> Result<EntityResponse, DispatchError> {
    state
        .dispatch_tenant_action_core(
            &sent.tenant,
            &sent.entity_type,
            &sent.entity_id,
            &sent.action,
            sent.params.clone(),
            &sent.agent_ctx,
            false,
            None,
        )
        .await
}

/// Send an owed reaction again; once it lands, run its cascade at the next
/// depth, as the dispatcher does for a reaction that lands at once.
fn react_again(state: ServerState, sent: Arc<OwedMove>, depth: u32) -> OwedTry {
    Box::pin(async move {
        let reply = send_reaction(&state, &sent).await;
        let attempt = Attempt::of(&reply);
        let dispatcher = state
            .reaction_dispatcher
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let (Ok(response), Some(dispatcher)) = (reply, dispatcher)
            && response.success
        {
            let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
            Box::pin(dispatcher.dispatch_reactions(
                &state,
                &sent.tenant,
                &sent.entity_type,
                &sent.entity_id,
                response.action(&sent.action),
                &response.state.status,
                &fields,
                depth + 1,
                &sent.agent_ctx,
            ))
            .await;
        }
        attempt
    })
}
