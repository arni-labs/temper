//! The cascade of a reaction's move that landed on its target.

use super::owed_writes::OwedMove;
use crate::entity_actor::EntityResponse;
use crate::state::ServerState;
use crate::trigger::ReactionResult;
use crate::trigger::dispatcher::CommittedMove;

impl ServerState {
    /// Run the reactions `sent`, a reaction's move, owes now it has landed
    /// (`response`), at the next `depth`: those its actor took from the table
    /// it applied the move under, which the move's follow-on row keeps until
    /// they are sent, so a restart before then sends them from the row. They
    /// run under the reaction's context, which knows the children the move
    /// spawned as made. A move answered by another recorded action runs that
    /// action's, read from its spec.
    pub(crate) async fn reaction_cascade(
        &self,
        sent: &OwedMove,
        response: &EntityResponse,
        depth: u32,
    ) -> Vec<ReactionResult> {
        let action = response.action(&sent.action);
        // The row the core dispatch holds for this caller.
        let follow = response.caller_holds.clone();
        let mut cascade_ctx = sent.agent_ctx.clone().having_made(
            response
                .spawn_requests
                .iter()
                .map(|spawn| (spawn.entity_type.as_str(), spawn.entity_id.as_str())),
        );
        if follow.is_some() {
            cascade_ctx.follow_ons = follow.clone();
        }
        let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
        let moved = CommittedMove {
            entity_type: &sent.entity_type,
            entity_id: &sent.entity_id,
            action,
            to_state: &response.state.status,
            fields: &fields,
            depth: depth + 1,
        };
        let declared = (action == sent.action).then_some(response.reactions.as_slice());
        let results = self
            .dispatch_declared_reactions(&sent.tenant, &moved, declared, &cascade_ctx)
            .await;
        if let Some(row) = &follow {
            self.settle_follow_row(row);
        }
        results
    }
}
