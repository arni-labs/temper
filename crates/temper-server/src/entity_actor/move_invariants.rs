//! Check a move against its type's `[[invariant]]`s
//! ([`temper_jit::table::invariant`]): refuse a move that would break one that
//! held, and warn about those the record already breaks.

use temper_jit::table::{EvalContext, Refusal, RelatedMap, TransitionTable};
use temper_spec::predicate::Name;

use super::types::EntityState;

/// The type's invariants as they stood when a move started.
pub(super) struct InvariantsBefore {
    status: String,
    ctx: EvalContext,
}

impl InvariantsBefore {
    /// Read what the invariants need from `state`, or `None` when the type
    /// has none, so a move of such a type costs nothing.
    pub(super) fn of(
        state: &EntityState,
        table: &TransitionTable,
        related: &RelatedMap,
    ) -> Option<Self> {
        (!table.invariants.is_empty()).then(|| Self {
            status: state.status.clone(),
            ctx: invariant_context(state, table, related),
        })
    }

    /// Why the move that left `state` must be refused: it broke an invariant
    /// that held when it started. The invariants the record broke already
    /// are logged and do not stop it.
    pub(super) fn refusal(
        &self,
        state: &EntityState,
        table: &TransitionTable,
        related: &RelatedMap,
        action: &str,
    ) -> Option<(String, Refusal)> {
        let after = invariant_context(state, table, related);
        let across =
            table.invariants_across_move((&self.status, &self.ctx), (&state.status, &after));
        if let Some((invariant, failure)) = across.broken {
            let mut message = format!(
                "Action '{action}' from state '{}' would break invariant '{}': '{}' would be false",
                self.status, invariant.name, failure.expr
            );
            if !failure.found.is_empty() {
                let found: Vec<String> = failure
                    .found
                    .iter()
                    .map(|(name, value)| format!("{name} = {value}"))
                    .collect();
                message.push_str(&format!(" ({})", found.join(", ")));
            }
            return Some((
                message,
                Refusal::InvariantViolated {
                    invariant: invariant.name.clone(),
                    assert: failure.expr,
                    conditions: failure.conditions,
                },
            ));
        }
        for (invariant, failure) in across.already_broken {
            tracing::warn!(
                entity_type = %state.entity_type,
                entity_id = %state.entity_id,
                action,
                status = %self.status,
                invariant = %invariant.name,
                failing = %failure.expr,
                "record already breaks this invariant, so the move was not held to it"
            );
        }
        None
    }
}

/// The first of the type's invariants `state` breaks, as its name and the
/// failing part with the values read. Those that read another record's status
/// are not checked: the record alone cannot answer them.
pub(crate) fn broken_invariant(state: &EntityState, table: &TransitionTable) -> Option<String> {
    let ctx = invariant_context(state, table, &RelatedMap::new());
    let (invariant, failure) = table.broken_invariant(&state.status, &ctx)?;
    let mut message = format!(
        "invariant '{}': '{}' is false",
        invariant.name, failure.expr
    );
    if !failure.found.is_empty() {
        let found: Vec<String> = failure
            .found
            .iter()
            .map(|(name, value)| format!("{name} = {value}"))
            .collect();
        message.push_str(&format!(" ({})", found.join(", ")));
    }
    Some(message)
}

/// The values the type's invariants read from `state`, and nothing else: a
/// check costs what the invariants read, not what the record holds.
fn invariant_context(
    state: &EntityState,
    table: &TransitionTable,
    related: &RelatedMap,
) -> EvalContext {
    let mut ctx = EvalContext {
        related: related.clone(),
        ..EvalContext::default()
    };
    for invariant in &table.invariants {
        invariant.assert.for_each_name(&mut |name| {
            let Name::Var(var) = name else { return };
            if let Some(value) = state.counters.get(var) {
                ctx.counters.insert(var.to_string(), *value);
            } else if var == "items" {
                // The legacy item count; a counter named `items` wins.
                ctx.counters.insert(var.to_string(), state.item_count);
            } else if let Some(value) = state.booleans.get(var) {
                ctx.booleans.insert(var.to_string(), *value);
            } else if let Some(value) = state.lists.get(var) {
                ctx.lists.insert(var.to_string(), value.clone());
            } else if let Some(value) = state.fields.get(var) {
                ctx.fields.insert(var.to_string(), value.clone());
            }
        });
    }
    ctx
}
