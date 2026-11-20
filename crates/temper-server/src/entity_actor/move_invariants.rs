//! Check a write against its type's `[[invariant]]`s and
//! `[[field_invariant]]`s ([`temper_jit::table::invariant`]): refuse a move or
//! field update that would break one that held, whoever makes it, and warn
//! about those the record already breaks.

use futures_util::FutureExt;
use serde_json::Value;
use std::collections::BTreeMap;

use temper_jit::table::{
    ConstraintDetails, ConstraintKind, DeclaredAs, EvalContext, GuardFailure, MoveInvariant,
    Refusal, RelatedMap, TransitionTable,
};
use temper_spec::predicate::Name;

use super::types::EntityState;

/// The type's invariants as they stood when a move started.
pub(super) struct InvariantsBefore {
    status: String,
    ctx: EvalContext,
    /// Stored representations replaced by verified logical values.
    stored_blobs: BTreeMap<String, Value>,
}

impl InvariantsBefore {
    /// Read what the invariants need from `state`, or `None` when the type
    /// has none, so a move of such a type costs nothing.
    pub(super) fn of(
        state: &EntityState,
        table: &TransitionTable,
        related: &RelatedMap,
        logical_fields: Option<&Value>,
    ) -> Result<Option<Self>, String> {
        if table.invariants.is_empty() {
            return Ok(None);
        }
        let mut ctx = invariant_context(state, table, related);
        let mut stored_blobs = BTreeMap::new();
        for (name, value) in &mut ctx.fields {
            if let Some(logical) = logical_fields.and_then(|fields| fields.get(name))
                && value != logical
            {
                stored_blobs.insert(name.clone(), value.clone());
                *value = logical.clone();
            }
        }
        resolve_pending_fields(&mut ctx, &[])?;
        Ok(Some(Self {
            status: state.status.clone(),
            ctx,
            stored_blobs,
        }))
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
        pending_blobs: &[crate::blobs::OverflowBlobWrite],
    ) -> Result<Option<(String, Refusal)>, String> {
        let mut after = invariant_context(state, table, related);
        // Reuse verified prestate bytes only while the stored value is unchanged.
        // New references must resolve from this move's pending writes.
        for (name, stored) in &self.stored_blobs {
            if after.fields.get(name) == Some(stored) {
                after
                    .fields
                    .insert(name.clone(), self.ctx.fields[name].clone());
            }
        }
        resolve_pending_fields(&mut after, pending_blobs)?;
        let across =
            table.invariants_across_move((&self.status, &self.ctx), (&state.status, &after));
        if let Some((invariant, failure)) = across.broken {
            let write = format!("Action '{action}' from state '{}'", self.status);
            return Ok(Some(broken_rule(
                state,
                invariant,
                failure,
                &write,
                operation(action),
            )));
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
        Ok(None)
    }
}

/// Hydrate locally generated writes through the same verified decoder as stored
/// blobs. With no external store this future always completes synchronously.
fn resolve_pending_fields(
    ctx: &mut EvalContext,
    pending: &[crate::blobs::OverflowBlobWrite],
) -> Result<(), String> {
    // Primitive values cannot contain references; keep the ordinary inline
    // move free of hydration allocations.
    if ctx
        .fields
        .values()
        .all(|value| !value.is_object() && !value.is_array())
    {
        return Ok(());
    }
    let mut fields = Value::Object(std::mem::take(&mut ctx.fields).into_iter().collect());
    let result = crate::blobs::hydrate_comparison_fields(
        &crate::blobs::BlobReadSource::Staged {
            store: None,
            legacy: None,
            blobs: pending,
        },
        &mut fields,
    )
    .now_or_never()
    .expect("invariant pending blob reads must be memory-only");
    let Value::Object(fields) = fields else {
        unreachable!("hydration preserves the selected fields object")
    };
    ctx.fields = fields.into_iter().collect();
    result
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
    if let Some(says) = &invariant.message {
        message.push_str(&format!(" ({says})"));
    }
    push_found(&mut message, &failure);
    Some(message)
}

/// Why a new record `state` may not be created: it would start out breaking
/// one of its type's invariants. A create has no state before it, so every
/// invariant must hold, as the verification cascade asks of a first state.
pub(crate) fn create_refusal(
    state: &EntityState,
    table: &TransitionTable,
) -> Option<(String, Refusal)> {
    let ctx = invariant_context(state, table, &RelatedMap::new());
    let (invariant, failure) = table.broken_invariant(&state.status, &ctx)?;
    let write = format!("Creating it in state '{}'", state.status);
    Some(broken_rule(state, invariant, failure, &write, "create"))
}

/// The label a write carries in a broken data rule: `patch` or `put` for a
/// field update, `action` for a move.
fn operation(action: &str) -> &'static str {
    match action {
        super::effects::FIELDS_UPDATED_EVENT => "patch",
        super::effects::FIELDS_REPLACED_EVENT => "put",
        _ => "action",
    }
}

/// The answer to a `write` on `state` that `invariant` refuses. A field
/// invariant answers as the broken data rule it always answered as, with the
/// spec's message; an `[[invariant]]` names the failing part and the values
/// the write would leave.
fn broken_rule(
    state: &EntityState,
    invariant: &MoveInvariant,
    failure: GuardFailure,
    write: &str,
    operation: &str,
) -> (String, Refusal) {
    if invariant.declared_as == DeclaredAs::FieldInvariant {
        let message = invariant.message.clone().unwrap_or_else(|| {
            format!(
                "field invariant '{}' violated on {}('{}')",
                invariant.name, state.entity_type, state.entity_id
            )
        });
        let details = ConstraintDetails {
            kind: ConstraintKind::FieldInvariant,
            invariant: Some(invariant.name.clone()),
            entity_type: state.entity_type.clone(),
            entity_id: state.entity_id.clone(),
            operation: operation.to_string(),
        };
        return (
            message,
            Refusal::ConstraintViolation {
                details: Box::new(details),
            },
        );
    }
    let mut message = format!(
        "{write} would break invariant '{}': '{}' would be false",
        invariant.name, failure.expr
    );
    push_found(&mut message, &failure);
    (
        message,
        Refusal::InvariantViolated {
            invariant: invariant.name.clone(),
            assert: failure.expr,
            conditions: failure.conditions,
        },
    )
}

/// Add the values a failing part read to `message`.
fn push_found(message: &mut String, failure: &GuardFailure) {
    if failure.found.is_empty() {
        return;
    }
    let found: Vec<String> = failure
        .found
        .iter()
        .map(|(name, value)| format!("{name} = {value}"))
        .collect();
    message.push_str(&format!(" ({})", found.join(", ")));
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
            } else if var == "items"
                && let Some(items) = super::effects::stored_item_count(state, &table.initial_values)
            {
                // A missing legacy count must leave declared defaults visible.
                ctx.counters.insert(var.to_string(), items);
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
