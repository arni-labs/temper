//! Checks on `[[carry]]` sections.

use crate::predicate::{Scope, VarKind, check};

use super::super::parser::AutomatonParseError;
use super::super::types::{Automaton, CARRY_PREFIX};

/// Each carry ends in a state of this version, reads only the record its
/// guard is about, and sets declared state variables to values of their
/// type. No action may take a carry's name.
pub(super) fn validate_carries(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    let invalid =
        |slot: &str, error: String| AutomatonParseError::Validation(format!("{slot}: {error}"));
    if let Some(action) = automaton
        .actions
        .iter()
        .find(|action| action.name.starts_with(CARRY_PREFIX))
    {
        return Err(invalid(
            &format!("action '{}'", action.name),
            format!("names starting with '{CARRY_PREFIX}' belong to [[carry]] moves"),
        ));
    }
    for (index, carry) in automaton.carries.iter().enumerate() {
        let slot = format!("carry #{} (from '{}')", index + 1, carry.from);
        if !automaton
            .automaton
            .states
            .iter()
            .any(|state| state == carry.target())
        {
            return Err(invalid(
                &slot,
                format!(
                    "carried records would end in '{}', which is not a state of this version; \
                     set `to` to one",
                    carry.target()
                ),
            ));
        }
        check(&carry.guard, Scope::Fields).map_err(|e| invalid(&slot, e))?;
        if carry.guard.reads_related_field() {
            return Err(invalid(
                &slot,
                format!(
                    "`{}` reads a related entity's field, which only an [[invariant]] may do",
                    carry.guard
                ),
            ));
        }
        for (name, value) in &carry.set {
            let Some(var) = automaton.state.iter().find(|var| &var.name == name) else {
                return Err(invalid(
                    &slot,
                    format!("sets '{name}', which this version does not declare"),
                ));
            };
            let fits = match var.var_type.kind() {
                VarKind::Counter => value.as_u64().is_some(),
                VarKind::Bool => value.is_boolean(),
                VarKind::Str => value.is_string(),
                VarKind::Num => value.as_i64().is_some(),
                VarKind::List => false,
            };
            if !fits {
                return Err(invalid(
                    &slot,
                    format!("cannot set '{name}' ({:?}) to {value}", var.var_type),
                ));
            }
        }
    }
    Ok(())
}
