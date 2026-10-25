//! Pure validation shared by native execution and deterministic simulation.

use super::TransitionTable;
use super::refusal::{ContractError, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::automaton::ActionConstraint;

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
    /// The related records `action`'s constraints compare against, as
    /// `(entity_type, id_field, field)`.
    pub fn constraint_related_refs(&self, action: &str) -> Vec<(String, String, String)> {
        self.action_contracts
            .get(action)
            .into_iter()
            .flat_map(|contract| &contract.constraints)
            .filter_map(ActionConstraint::related_field)
            .map(|(entity, id_field, field)| {
                (entity.to_string(), id_field.to_string(), field.to_string())
            })
            .collect()
    }

    /// Check `action`'s `param_equals_related_field` constraints against the
    /// related records read for this dispatch. A record that was not read,
    /// was not found, or has no such field refuses the action: the value
    /// cannot be shown to match.
    pub fn validate_related_params(
        &self,
        action: &str,
        params: &Value,
        related: &super::RelatedMap,
    ) -> Result<(), ContractError> {
        let Some(contract) = self.action_contracts.get(action) else {
            return Ok(());
        };
        for constraint in &contract.constraints {
            let Some((entity, id_field, field)) = constraint.related_field() else {
                continue;
            };
            let name = constraint.param();
            let refused = |message: String| ContractError {
                message,
                refusal: Some(Refusal::ParameterConstraint {
                    parameter: name.to_string(),
                }),
            };
            let Some(param) = params.get(name) else {
                return Err(ContractError {
                    message: format!("Action '{action}' requires parameter '{name}'"),
                    refusal: Some(Refusal::MissingParameter {
                        parameter: name.to_string(),
                    }),
                });
            };
            let key = (entity.to_string(), id_field.to_string());
            let records = match related.get(&key) {
                Some(super::Related::Records(records)) if !records.is_empty() => records,
                _ => {
                    return Err(refused(format!(
                        "Action '{action}' parameter '{name}' must equal {entity}[{id_field}].{field}, and no such record was found"
                    )));
                }
            };
            let matches = records.iter().all(|record| {
                record
                    .as_ref()
                    .and_then(|record| record.fields.get(field))
                    .is_some_and(|value| matches_field(param, value, false) == Some(true))
            });
            if !matches {
                return Err(refused(format!(
                    "Action '{action}' parameter '{name}' must equal {entity}[{id_field}].{field}"
                )));
            }
        }
        Ok(())
    }

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
    ) -> Result<(), ContractError> {
        let contract = self.action_contracts.get(action);
        if !self.strict_action_params
            && contract.is_none_or(|contract| contract.constraints.is_empty())
        {
            return Ok(());
        }
        let object = params.as_object().ok_or_else(|| {
            ContractError::message("Action parameters must be a JSON object".into())
        })?;
        let contract = contract.ok_or_else(|| {
            ContractError::message(format!(
                "Action '{action}' has no declared parameter contract"
            ))
        })?;
        if self.strict_action_params {
            for key in object.keys() {
                if !contract.params.contains(key) {
                    return Err(ContractError::message(format!(
                        "Action '{action}' does not accept parameter '{key}'"
                    )));
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
                    return Err(ContractError::message(format!(
                        "Action '{action}' parameter '{name}' has an invalid type"
                    )));
                }
            }
        }
        for constraint in &contract.constraints {
            let name = constraint.param();
            let param = object.get(name).ok_or_else(|| ContractError {
                message: format!("Action '{action}' requires parameter '{name}'"),
                refusal: Some(Refusal::MissingParameter {
                    parameter: name.to_string(),
                }),
            })?;
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
                // Compared with the related record by `validate_related_params`,
                // which has it.
                ActionConstraint::ParamEqualsRelatedField { .. } => true,
            };
            if !passed {
                return Err(ContractError {
                    message: format!("Action '{action}' constraint failed for parameter '{name}'"),
                    refusal: Some(Refusal::ParameterConstraint {
                        parameter: name.to_string(),
                    }),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "action_contract_test.rs"]
mod tests;
