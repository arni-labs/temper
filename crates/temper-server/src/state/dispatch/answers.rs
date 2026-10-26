//! A call's answer: which of its trigger's `on_success` actions the kernel
//! sends with the call's values.
//!
//! A module computes values; the spec's guards decide the move. The kernel
//! sends the first listed action the record takes with the values (its
//! `from` admits the status, its constraints hold, and its guard holds
//! reading them as `params.p`). When none does, the last is sent, and its
//! refusal is recorded as a single `on_success`'s always was. The record
//! decides again when the answer commits, so a move between this choice and
//! the commit cannot make it take an action its guard refuses.

use super::WasmEntityRef;
use crate::entity_actor::effects::{build_eval_context_with_xref, with_params};

impl crate::state::ServerState {
    /// The action of `actions` to send with `params`: the first the record
    /// takes now, or else the last. `None` only when `actions` is empty.
    pub(super) async fn choose_answer<'a>(
        &self,
        entity_ref: WasmEntityRef<'_>,
        actions: &'a [String],
        params: &serde_json::Value,
    ) -> Option<&'a str> {
        let (last, earlier) = actions.split_last()?;
        for action in earlier {
            if self.takes(entity_ref, action, params).await {
                return Some(action);
            }
        }
        Some(last)
    }

    /// Whether the record would take `action` with `params` now.
    async fn takes(
        &self,
        entity_ref: WasmEntityRef<'_>,
        action: &str,
        params: &serde_json::Value,
    ) -> bool {
        let Ok(table) =
            self.transition_table_for_dispatch(entity_ref.tenant, entity_ref.entity_type)
        else {
            return false;
        };
        let Ok(current) = self
            .get_tenant_entity_state(
                entity_ref.tenant,
                entity_ref.entity_type,
                entity_ref.entity_id,
            )
            .await
        else {
            return false;
        };
        let state = &current.state;
        if table
            .validate_action_params(
                action,
                params,
                &state.fields,
                &state.counters,
                &state.booleans,
            )
            .is_err()
        {
            return false;
        }
        let related = self
            .resolve_cross_entity_guards(
                entity_ref.tenant,
                entity_ref.entity_type,
                entity_ref.entity_id,
                action,
            )
            .await;
        let mut ctx = build_eval_context_with_xref(state, &related, &table, Some(action));
        with_params(&mut ctx, params);
        table
            .evaluate_ctx(&state.status, &ctx, action)
            .is_some_and(|result| result.success)
    }
}
