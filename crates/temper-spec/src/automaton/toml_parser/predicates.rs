//! Reading the four predicate slots: action `guard`, trigger `guard`,
//! `[[invariant]] assert` and `[[field_invariant]] assert`. Each holds one
//! string in the [`crate::predicate`] grammar.

use super::super::field_invariant::FieldInvariant;
use super::super::legacy;
use super::super::parser::AutomatonParseError;
use super::super::types::{ActionTrigger, Invariant};
use super::{MIGRATE_HINT, RESOLVER_RETIRED, TRIGGER_RETIRED, entries, retired, typed};
use crate::predicate::{self, Expr};
use toml::{Table, Value};

fn invalid(slot: &str, message: impl std::fmt::Display) -> AutomatonParseError {
    AutomatonParseError::Validation(format!("{slot}: {message}"))
}

/// One predicate string.
fn expression(slot: &str, value: &Value, legacy: bool) -> Result<Expr, AutomatonParseError> {
    let Value::String(source) = value else {
        return Err(invalid(
            slot,
            format!(
                "must be an expression string; this is the old predicate syntax; {MIGRATE_HINT}"
            ),
        ));
    };
    predicate::parse(source).map_err(|e| {
        if legacy {
            invalid(
                slot,
                format!("{e}; this is the old predicate syntax; {MIGRATE_HINT}"),
            )
        } else {
            invalid(slot, e)
        }
    })
}

/// An action `guard`.
pub(super) fn action_guard(action: &str, value: &Value) -> Result<Expr, AutomatonParseError> {
    expression(
        &format!("action '{action}' guard"),
        value,
        legacy::is_legacy_guard(value),
    )
}

/// Every `[[invariant]]`: a `name` and an `assert`.
pub(super) fn invariants(doc: &Table) -> Result<Vec<Invariant>, AutomatonParseError> {
    entries(doc, "invariant", "name", invariant)
}

fn invariant(slot: &str, table: &Table) -> Result<Invariant, AutomatonParseError> {
    if table.contains_key("when") {
        return Err(invalid(
            slot,
            format!("`when` is not a key; this is the old predicate syntax; {MIGRATE_HINT}"),
        ));
    }
    if let Some(key) = table
        .keys()
        .find(|key| !matches!(key.as_str(), "name" | "assert" | "lemma"))
    {
        return Err(invalid(
            slot,
            format!("unknown field `{key}`, expected `name`, `assert` or `lemma`"),
        ));
    }
    let name = table
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = table
        .get("assert")
        .ok_or_else(|| invalid(slot, "missing `assert`"))?;
    if let Some(text) = value.as_str()
        && matches!(
            legacy::invariant_to_expr(&[], text),
            Ok(legacy::LoweredInvariant::Terminal(_) | legacy::LoweredInvariant::Dropped(_))
        )
    {
        return Err(invalid(
            slot,
            format!(
                "`{text}` is not an invariant; terminal states go in `[automaton] terminal`; this is the old predicate syntax; {MIGRATE_HINT}"
            ),
        ));
    }
    let legacy = value.as_str().is_some_and(|text| {
        predicate::parse(text).is_err() && legacy::invariant_to_expr(&[], text).is_ok()
    });
    let assert = expression(slot, value, legacy)?;
    // MUTANT: an invariant's lemmas are not read.
    let lemma = match None::<&toml::Value> {
        Some(lemmas) => typed(lemmas.clone(), &format!("{slot} lemma"))?,
        None => Vec::new(),
    };
    Ok(Invariant {
        name: name.to_string(),
        assert,
        lemma,
    })
}

/// Every `[[field_invariant]]`.
pub(super) fn field_invariants(doc: &Table) -> Result<Vec<FieldInvariant>, AutomatonParseError> {
    entries(doc, "field_invariant", "name", |slot, table| {
        if table.contains_key("when") || table.contains_key("require") {
            return Err(invalid(
                slot,
                format!(
                    "`when`/`require` are not keys, write one `assert`; this is the old predicate syntax; {MIGRATE_HINT}"
                ),
            ));
        }
        typed(Value::Table(table.clone()), slot)
    })
}

/// An action's `[[action.triggers]]`; `action` names the action in errors.
pub(super) fn triggers(
    action: &str,
    value: &Value,
) -> Result<Vec<ActionTrigger>, AutomatonParseError> {
    let Value::Array(entries) = value else {
        return Err(AutomatonParseError::Toml(format!(
            "{action}: 'triggers' must be written as [[action.triggers]]"
        )));
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
        let slot = format!("{action} trigger '{name}'");
        if let Some(guard) = entry.get("guard")
            && !guard.is_str()
        {
            return Err(invalid(
                &format!("{slot} guard"),
                format!(
                    "must be an expression string; this is the old predicate syntax; {MIGRATE_HINT}"
                ),
            ));
        }
        if let Value::Table(table) = entry {
            retired(&slot, table, TRIGGER_RETIRED)?;
            if let Some(Value::Table(resolver)) = table.get("resolve_target") {
                retired(
                    &format!("{slot} resolve_target"),
                    resolver,
                    RESOLVER_RETIRED,
                )?;
            }
        }
        out.push(typed(entry.clone(), &slot)?);
    }
    Ok(out)
}
