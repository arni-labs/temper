//! Pure validation shared by native execution and deterministic simulation.

use super::TransitionTable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::automaton::ActionConstraint;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_initial_identity_is_a_nonempty_string() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Identity"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
"#,
        );
        for invalid in [
            json!({"x":1}),
            json!([]),
            json!(1),
            json!(false),
            Value::Null,
            json!(""),
        ] {
            for key in ["Id", "id"] {
                assert!(
                    table
                        .validate_initial_fields(&json!({key:invalid}))
                        .is_err()
                );
            }
        }
        assert!(
            table
                .validate_initial_fields(&json!({"Id":"valid","id":"valid"}))
                .is_ok()
        );
    }

    #[test]
    fn constraints_do_not_recreate_missing_persisted_fields_from_defaults() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "PersistedSequence"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "sequence"
type = "counter"
initial = 7
[[action]]
name = "Advance"
kind = "input"
from = ["Active"]
params = ["expected"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected"
field = "sequence"
"#,
        );
        let params = json!({"expected":7});
        let mut fields = json!({});
        let mut counters = BTreeMap::new();
        let mut booleans = BTreeMap::new();
        assert!(
            table
                .validate_action_params("Advance", &params, &fields, &counters, &booleans)
                .is_err()
        );
        table.initialize_declared_fields(&mut fields, &mut counters, &mut booleans);
        assert!(
            table
                .validate_action_params("Advance", &params, &fields, &counters, &booleans)
                .is_ok()
        );
        counters.insert("sequence".into(), 8);
        assert!(
            table
                .validate_action_params("Advance", &params, &fields, &counters, &booleans)
                .is_err()
        );
    }

    #[test]
    fn greater_than_requires_a_nonnegative_parameter_even_for_signed_state() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "SignedSequence"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "sequence"
type = "int"
initial = -3
[[action]]
name = "Advance"
kind = "input"
from = ["Active"]
params = ["expected"]
[[action.constraints]]
kind = "param_greater_than_field"
param = "expected"
field = "sequence"
"#,
        );
        let fields = json!({"sequence":-3});
        let counters = BTreeMap::new();
        let booleans = BTreeMap::new();
        assert!(
            table
                .validate_action_params(
                    "Advance",
                    &json!({"expected":-2}),
                    &fields,
                    &counters,
                    &booleans
                )
                .is_err()
        );
        assert!(
            table
                .validate_action_params(
                    "Advance",
                    &json!({"expected":0}),
                    &fields,
                    &counters,
                    &booleans
                )
                .is_ok()
        );
    }
}

/// Allowed parameters and atomic pre-state checks for one action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionContract {
    /// Incoming keys explicitly declared in the IOA action.
    pub params: BTreeSet<String>,
    /// Explicit types only; bare parameter names retain constraint-inferred semantics.
    #[serde(default)]
    pub param_types: BTreeMap<String, temper_spec::automaton::VarType>,
    /// Values resolved by authenticated dispatch, never accepted from the caller.
    #[serde(default)]
    pub param_sources: BTreeMap<String, temper_spec::automaton::ParameterSource>,
    /// Preconditions checked before effects and field synchronization.
    pub constraints: Vec<ActionConstraint>,
}

/// Typed initial values, retained when a transition table is persisted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitialValues {
    /// Natural-number counters.
    pub counters: BTreeMap<String, usize>,
    /// Boolean state variables.
    pub booleans: BTreeMap<String, bool>,
    /// List and set state variables.
    pub lists: BTreeMap<String, Vec<String>>,
    /// Ordinary field values.
    pub fields: BTreeMap<String, Value>,
}

impl InitialValues {
    pub(crate) fn from_declarations(state: &[temper_spec::automaton::StateVar]) -> Self {
        let mut values = Self::default();
        for var in state {
            use temper_spec::automaton::Initial;
            match &var.initial {
                Initial::Counter(n) => {
                    values.counters.insert(var.name.clone(), *n);
                }
                Initial::Bool(b) => {
                    values.booleans.insert(var.name.clone(), *b);
                }
                Initial::List(items) => {
                    values.lists.insert(var.name.clone(), items.clone());
                }
                Initial::String(_) | Initial::Int(_) => {
                    values
                        .fields
                        .insert(var.name.clone(), var.initial.to_json());
                }
            }
        }
        values
    }
}

fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

fn matches_field(param: &Value, field: &Value, counter: bool) -> Option<bool> {
    match (param, field) {
        (Value::Number(_), Value::Number(_)) => {
            if counter && param.as_u64().is_none() {
                return None;
            }
            Some(integer(param)? == integer(field)?)
        }
        (Value::String(_), Value::String(_))
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Array(_), Value::Array(_))
        | (Value::Object(_), Value::Object(_)) => Some(param == field),
        _ => None,
    }
}

impl TransitionTable {
    /// Whether any action requires the persisted input-contract semantics.
    pub fn has_input_contracts(&self) -> bool {
        self.strict_action_params
            || self
                .action_contracts
                .values()
                .any(|contract| !contract.constraints.is_empty())
    }

    /// Materialize declarations once when creating a fresh contracted actor.
    /// Recovery must retain persisted state, including missing fields.
    pub fn initialize_declared_fields(
        &self,
        fields: &mut Value,
        counters: &mut BTreeMap<String, usize>,
        booleans: &mut BTreeMap<String, bool>,
    ) {
        if !self.strict_action_params
            && self
                .action_contracts
                .values()
                .all(|contract| contract.constraints.is_empty())
        {
            return;
        }
        counters.extend(self.initial_values.counters.clone());
        booleans.extend(self.initial_values.booleans.clone());
        if fields.is_null() {
            *fields = Value::Object(Default::default());
        }
        if let Some(fields) = fields.as_object_mut() {
            for (name, value) in &self.initial_values.fields {
                fields.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }

    /// Strict entities begin with identity only; data enters through declared actions.
    pub fn validate_initial_fields(&self, fields: &Value) -> Result<(), String> {
        if !self.strict_action_params {
            return Ok(());
        }
        let fields = fields
            .as_object()
            .ok_or_else(|| "Entity creation requires a JSON object".to_owned())?;
        for (key, value) in fields {
            match key.as_str() {
                "id" | "Id" => {
                    if value.as_str().is_none_or(str::is_empty) {
                        return Err("Strict entity identity must be a nonempty string".into());
                    }
                }
                "status" | "Status" if value.as_str() == Some(self.initial_state.as_str()) => {}
                _ => {
                    return Err(format!(
                        "Strict entity creation does not accept field '{key}'; use a declared action"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Check request parameters against the unmodified actor state.
    /// Values are never included in errors because parameters can contain secrets.
    pub fn validate_action_params(
        &self,
        action: &str,
        params: &Value,
        fields: &Value,
        counters: &BTreeMap<String, usize>,
        booleans: &BTreeMap<String, bool>,
    ) -> Result<(), String> {
        let contract = self.action_contracts.get(action);
        if !self.strict_action_params
            && contract.is_none_or(|contract| contract.constraints.is_empty())
        {
            return Ok(());
        }
        let object = params
            .as_object()
            .ok_or_else(|| "Action parameters must be a JSON object".to_owned())?;
        let contract = contract
            .ok_or_else(|| format!("Action '{action}' has no declared parameter contract"))?;
        if self.strict_action_params {
            for key in object.keys() {
                if !contract.params.contains(key) {
                    return Err(format!(
                        "Action '{action}' does not accept parameter '{key}'"
                    ));
                }
            }
        }
        for (name, kind) in &contract.param_types {
            if let Some(value) = object.get(name) {
                use temper_spec::automaton::VarType;
                let valid = match kind {
                    VarType::String => value.is_string(),
                    VarType::Bool => value.is_boolean(),
                    VarType::Int => value.as_i64().is_some(),
                    VarType::Counter => value.as_u64().is_some(),
                    VarType::List => value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string)),
                };
                if !valid {
                    return Err(format!(
                        "Action '{action}' parameter '{name}' has an invalid type"
                    ));
                }
            }
        }
        for constraint in &contract.constraints {
            let name = constraint.param();
            let param = object
                .get(name)
                .ok_or_else(|| format!("Action '{action}' requires parameter '{name}'"))?;
            let field = constraint.field().and_then(|name| {
                counters
                    .get(name)
                    .map(|value| Value::from(*value as u64))
                    .or_else(|| booleans.get(name).map(|value| Value::from(*value)))
                    .or_else(|| fields.get(name).cloned())
            });
            let counter = constraint.field().is_some_and(|name| {
                self.initial_values.counters.contains_key(name) || counters.contains_key(name)
            });
            let passed = match constraint {
                ActionConstraint::ParamNonempty { .. } => {
                    param.as_str().is_some_and(|value| !value.trim().is_empty())
                }
                ActionConstraint::ParamEqualsField { .. } => field
                    .as_ref()
                    .is_some_and(|field| matches_field(param, field, counter) == Some(true)),
                ActionConstraint::ParamNotEqualsField { .. } => field
                    .as_ref()
                    .is_some_and(|field| matches_field(param, field, counter) == Some(false)),
                ActionConstraint::ParamGreaterThanField { .. } => {
                    param.as_u64().is_some()
                        && matches!((integer(param), field.as_ref().and_then(integer)), (Some(a), Some(b)) if a > b)
                }
            };
            if !passed {
                return Err(format!(
                    "Action '{action}' constraint failed for parameter '{name}'"
                ));
            }
        }
        Ok(())
    }
}
