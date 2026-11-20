//! A move aimed at a record that does not exist, and the triggers of a
//! committed action that failed.
//!
//! An entity trigger whose id named no record used to make an empty record
//! for the id, be refused on it, and have that refusal thrown away: its
//! sender got a clean answer and the work the trigger was to start never
//! started. No record is made now for a move that cannot make one
//! ([`ServerState::refused_before_record`]), and a failed trigger is said
//! beside the action that sent it ([`ServerState::say_failed_reactions`]).

use temper_jit::table::{Refusal, TransitionTable};
use temper_runtime::tenant::TenantId;

use super::DispatchError;
use crate::entity_actor::EntityResponse;
use crate::trigger::ReactionResult;

impl crate::state::ServerState {
    /// The refusal of a first move on a record that does not exist, before
    /// an actor is started for it: the record as it would be made, the
    /// message and the typed refusal. `None` when the record exists or the
    /// move may go on to its actor.
    ///
    /// A record is made by a move that starts from its type's initial
    /// state, or by the kernel's own `Create`
    /// ([`crate::entity_actor::makes_record`]). Any other move aimed at a record that does not exist (a
    /// trigger whose id names no record, say) is refused here, so no empty
    /// record is written for it and the refusal says the record is missing
    /// ([`crate::entity_actor::no_record_refusal`]). A move that does make
    /// the record has its parameters checked against the initial state, as
    /// before.
    ///
    /// Cost: nothing for a record the server holds. Otherwise one check that
    /// the record exists, for a move that cannot make it or takes
    /// parameters.
    pub(super) async fn refused_before_record(
        &self,
        table: &TransitionTable,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: &serde_json::Value,
    ) -> Result<Option<(EntityResponse, String, Option<Refusal>)>, DispatchError> {
        let makes_record = crate::entity_actor::makes_record(table, action);
        if (makes_record && !table.has_input_contracts())
            || self.entity_exists(tenant, entity_type, entity_id)
        {
            return Ok(None);
        }
        let snapshot = self
            .load_authz_resource_snapshot(tenant, entity_type, entity_id)
            .await?;
        if snapshot.exists {
            return Ok(None);
        }
        let response = snapshot.current_state;
        let refused =
            match crate::entity_actor::no_record_refusal(table, entity_type, entity_id, action) {
                Some((message, refusal)) => Some((message, Some(refusal))),
                None => table
                    .validate_action_params(
                        action,
                        params,
                        &response.state.fields,
                        &response.state.counters,
                        &response.state.booleans,
                    )
                    .err()
                    .map(|error| (error.message, error.refusal)),
            };
        Ok(refused.map(|(message, refusal)| (response, message, refusal)))
    }

    /// Say beside the committed `response` each of its entity triggers that
    /// failed: its target missing, refused by its state or guard, withheld by
    /// Cedar, or its id unresolved. They go in the list a failed integration
    /// uses (`@temper.triggerFailures`), as `"<rule>: <why>"`, and in the
    /// reply kept for `idempotency_key`, so a retry answers the same.
    ///
    /// `<why>` is the reaction's sentence for the sender
    /// ([`ReactionResult::said`]): the sender may not be allowed to read the
    /// target, so it never carries the target's id, state or values.
    pub(super) fn say_failed_reactions(
        &self,
        actor_key: &str,
        idempotency_key: Option<&str>,
        response: &mut EntityResponse,
        reacted: &[ReactionResult],
    ) {
        let failed: Vec<String> = reacted
            .iter()
            .filter(|reaction| !reaction.success)
            .map(|reaction| {
                format!(
                    "{}: {}",
                    reaction.rule_name,
                    reaction.said.as_deref().unwrap_or("not made")
                )
            })
            .filter(|failure| !response.trigger_failures.contains(failure))
            .collect();
        if failed.is_empty() {
            return;
        }
        if let Some(key) = idempotency_key {
            self.idempotency_cache
                .add_trigger_failures(actor_key, key, &failed);
        }
        response.trigger_failures.extend(failed);
    }
}
