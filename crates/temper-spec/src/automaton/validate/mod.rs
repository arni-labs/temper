//! Checks a parsed spec must pass before it is installed: declared states,
//! predicates and effects, state timeouts, triggers, webhooks and vectors.

mod triggers;

use super::parser::AutomatonParseError;
use super::types::*;

pub(super) fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    // 1. Initial state must be in the states list.
    if !automaton
        .automaton
        .states
        .contains(&automaton.automaton.initial)
    {
        return Err(AutomatonParseError::Validation(format!(
            "initial state '{}' is not in states list",
            automaton.automaton.initial
        )));
    }

    // 2. Runtime-owned fields cannot also be mutable spec variables or action
    //    params. That would create a second identity/lifecycle/context truth.
    for state_var in &automaton.state {
        if super::types::is_server_derived_field_name(&state_var.name) {
            return Err(AutomatonParseError::Validation(format!(
                "state variable '{}' uses a runtime-owned field name",
                state_var.name
            )));
        }
    }
    for action in &automaton.actions {
        for param in &action.params {
            if param.source().is_some()
                && (!automaton.automaton.strict_action_params
                    || param.param_type() != VarType::String)
            {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' bound parameter '{}' requires strict_action_params and string type",
                    action.name,
                    param.name(),
                )));
            }
            if super::types::is_server_derived_field_name(param.name()) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' parameter '{}' uses a runtime-owned field name",
                    action.name,
                    param.name()
                )));
            }
        }
    }

    super::contracts::validate(automaton)?;
    validate_predicates(automaton)?;

    // 3. All `from` and `to` states in actions must be declared states.
    for action in &automaton.actions {
        for from in &action.from {
            if !automaton.automaton.states.contains(from) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references undeclared from-state '{from}'",
                    action.name
                )));
            }
        }
        if let Some(to) = &action.to
            && !automaton.automaton.states.contains(to)
        {
            return Err(AutomatonParseError::Validation(format!(
                "action '{}' references undeclared to-state '{to}'",
                action.name
            )));
        }
    }

    let action_names: Vec<&str> = automaton.actions.iter().map(|a| a.name.as_str()).collect();

    // 5. Validate [[state_timeout]] declarations (ADR-0049).
    //    - `state` must be a declared state.
    //    - `on_timeout` must be a declared action.
    //    - each `reset_on` entry must be a declared action.
    //    - `after_seconds` must be > 0 (a zero delay is almost certainly a typo).
    //    - `max_occurrences` must be >= 1.
    //    - the same state must not be declared twice (ambiguous timer contract).
    let mut seen_states: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for st in &automaton.state_timeouts {
        if !automaton.automaton.states.contains(&st.state) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout references undeclared state '{}'",
                st.state
            )));
        }
        if !seen_states.insert(st.state.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout declared twice for state '{}'",
                st.state
            )));
        }
        if st.after_seconds == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' must have after_seconds > 0",
                st.state
            )));
        }
        if st.max_occurrences == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' must have max_occurrences >= 1",
                st.state
            )));
        }
        if !action_names.contains(&st.on_timeout.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "state_timeout for '{}' references unknown on_timeout action '{}'",
                st.state, st.on_timeout
            )));
        }
        for reset in &st.reset_on {
            if !action_names.contains(&reset.as_str()) {
                return Err(AutomatonParseError::Validation(format!(
                    "state_timeout for '{}' references unknown reset_on action '{reset}'",
                    st.state
                )));
            }
        }
    }

    // 6. Validate allow_indefinite_states entries are declared states
    //    (ADR-0050 support).
    for state in &automaton.automaton.allow_indefinite_states {
        if !automaton.automaton.states.contains(state) {
            return Err(AutomatonParseError::Validation(format!(
                "allow_indefinite_states references undeclared state '{state}'"
            )));
        }
    }

    // 7. Validate [[action.triggers]] declarations (ADR-0046).
    triggers::validate_action_triggers(automaton, &action_names)?;
    triggers::validate_timeouts_and_webhooks(automaton)?;

    // 7. Validate [[vector]] access-path declarations (ADR-0155).
    //    - `property` and `model_property` must be declared state variables.
    //    - `dims` must be > 0.
    //    - `metric` must be one of cosine | dot | l2.
    //    - names must be unique (each identifies one index partition + `decl=`).
    validate_vector_decls(automaton)?;

    Ok(())
}

/// Name- and type-check every predicate, and check `terminal` states exist.
fn validate_predicates(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    use crate::predicate::{Scope, VarKind, check, check_effects};

    let vars: std::collections::BTreeMap<String, VarKind> = automaton
        .state
        .iter()
        .map(|sv| (sv.name.clone(), sv.var_type.kind()))
        .collect();
    let invalid =
        |slot: String, error: String| AutomatonParseError::Validation(format!("{slot}: {error}"));
    for action in &automaton.actions {
        check(&action.guard, Scope::State(&vars))
            .map_err(|e| invalid(format!("action '{}' guard", action.name), e))?;
        check_effects(&action.effect, &vars)
            .map_err(|e| invalid(format!("action '{}' effect", action.name), e))?;
        for effect in &action.effect {
            let (Effect::Schedule { action: target, .. }
            | Effect::ScheduleAt { action: target, .. }) = effect
            else {
                continue;
            };
            if !automaton.actions.iter().any(|a| &a.name == target) {
                return Err(invalid(
                    format!("action '{}' effect", action.name),
                    format!("'{effect}' schedules unknown action '{target}'"),
                ));
            }
        }
        for trigger in &action.triggers {
            if let Some(guard) = &trigger.guard {
                check(guard, Scope::Fields).map_err(|e| {
                    invalid(
                        format!(
                            "trigger '{}' on action '{}' guard",
                            trigger.name, action.name
                        ),
                        e,
                    )
                })?;
            }
        }
    }
    for inv in &automaton.invariants {
        let slot = format!("invariant '{}'", inv.name);
        check(&inv.assert, Scope::State(&vars)).map_err(|e| invalid(slot.clone(), e))?;
        if let Some(culprit) = crate::predicate::unmodelable(&inv.assert, &vars) {
            return Err(invalid(
                slot,
                format!(
                    "`{culprit}` reads a value the verification cascade cannot model \
                     (a string, number, field or related entity); state it as a \
                     [[field_invariant]], which is checked on writes"
                ),
            ));
        }
    }
    for inv in &automaton.field_invariants {
        check(&inv.assert, Scope::Fields)
            .map_err(|e| invalid(format!("field_invariant '{}'", inv.name), e))?;
    }
    for state in &automaton.automaton.terminal {
        if !automaton.automaton.states.contains(state) {
            return Err(AutomatonParseError::Validation(format!(
                "terminal references undeclared state '{state}'"
            )));
        }
    }
    Ok(())
}

/// Validate all `[[vector]]` access-path declarations per ADR-0155.
fn validate_vector_decls(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    const METRICS: [&str; 3] = ["cosine", "dot", "l2"];
    let state_var_names: std::collections::BTreeSet<&str> =
        automaton.state.iter().map(|sv| sv.name.as_str()).collect();
    let mut seen_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for vec_decl in &automaton.vectors {
        if !seen_names.insert(vec_decl.name.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' declared twice",
                vec_decl.name
            )));
        }
        if !state_var_names.contains(vec_decl.property.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' references undeclared property state variable '{}'",
                vec_decl.name, vec_decl.property
            )));
        }
        if !state_var_names.contains(vec_decl.model_property.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' references undeclared model_property state variable '{}'",
                vec_decl.name, vec_decl.model_property
            )));
        }
        if vec_decl.dims == 0 {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' must declare dims > 0",
                vec_decl.name
            )));
        }
        if !METRICS.contains(&vec_decl.metric.as_str()) {
            return Err(AutomatonParseError::Validation(format!(
                "vector path '{}' has unknown metric '{}' (expected one of cosine, dot, l2)",
                vec_decl.name, vec_decl.metric
            )));
        }
    }
    Ok(())
}
