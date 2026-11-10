//! Why an action was refused, as typed data beside the message callers read.
//!
//! The kernel's refusal messages are for people and agents to read. A
//! [`Refusal`] carries the same facts as fields, so a caller never parses the
//! message: it serializes with its `code` tag, flat beside the message, as in
//! `{"code":"GuardFailed","message":"...","guard":"holders <= 1","conditions":[...]}`.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why the kernel refused an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code")]
pub enum Refusal {
    /// The action's guard did not hold.
    GuardFailed {
        /// The failing part of the guard in canonical form, as the message names it.
        guard: String,
        /// One entry per comparison in `guard` that splits into a field, an
        /// operator and a wanted value. A call, `||`, arithmetic or a
        /// cross-entity status does not split and has no entry.
        conditions: Vec<GuardCondition>,
    },
    /// No rule for the action fires from the entity's current state.
    InvalidState {
        /// The entity's current state.
        state: String,
        /// The states the action may start from.
        from: Vec<String>,
    },
    /// A parameter failed its declared constraint.
    ParameterConstraint {
        /// The parameter's name.
        parameter: String,
    },
    /// A parameter a constraint reads was not sent.
    MissingParameter {
        /// The parameter's name.
        parameter: String,
    },
    /// The move would make one of the type's `[[invariant]]`s false, and it
    /// held before the move.
    InvariantViolated {
        /// The invariant's name, as the spec writes it.
        invariant: String,
        /// The failing part of its assertion, in canonical form.
        assert: String,
        /// One entry per comparison in `assert` that splits into a field, an
        /// operator and a wanted value, with the value the move would leave.
        conditions: Vec<GuardCondition>,
    },
    /// The record the move would store breaks one of the app's data rules:
    /// a field invariant, a cross invariant or relation integrity. A create
    /// or update that breaks one answers with the same code and details.
    ConstraintViolation {
        /// The rule broken and the record it was checked on. Boxed so the
        /// other refusals stay small.
        details: Box<ConstraintDetails>,
    },
    /// The record changed after the move was decided on it, so nothing was
    /// written. The move is decided again on the record as it is now.
    RecordChanged,
    /// The request's Idempotency-Key was already used on this entity for a
    /// different request.
    IdempotencyKeyReused {
        /// The action the key's request named.
        action: String,
    },
}

impl Refusal {
    /// The error code this refusal answers with.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::GuardFailed { .. } => "GuardFailed",
            Refusal::InvalidState { .. } => "InvalidState",
            Refusal::ParameterConstraint { .. } => "ParameterConstraint",
            Refusal::MissingParameter { .. } => "MissingParameter",
            Refusal::InvariantViolated { .. } => "InvariantViolated",
            Refusal::ConstraintViolation { .. } => "ConstraintViolation",
            Refusal::RecordChanged => "RecordChanged",
            Refusal::IdempotencyKeyReused { .. } => "IdempotencyKeyReused",
        }
    }
}

/// A broken data rule, and the record and write it was checked on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintDetails {
    /// Which kind of rule was broken.
    #[serde(rename = "type")]
    pub kind: ConstraintKind,
    /// The invariant's name, as the spec writes it; `None` for relation
    /// integrity and for a check that could not be finished.
    pub invariant: Option<String>,
    /// The record's entity type.
    pub entity_type: String,
    /// The record's id.
    pub entity_id: String,
    /// The write: `create`, `patch`, `put`, `delete` or `action`.
    pub operation: String,
}

/// The kinds of data rule a stored record is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    /// A relation field names a record that does not exist, or a delete
    /// would leave a reference to it.
    RelationIntegrity,
    /// A related or referencing record could not be checked.
    RelationUnverified,
    /// A `cross_invariant` over this record and a related one.
    CrossInvariant,
    /// A `[[field_invariant]]` over the record's own fields.
    FieldInvariant,
    /// A value the checks read could not be read back from where an earlier
    /// write stored it, so the record cannot be checked and is not written.
    StoredValueUnreadable,
}

/// One comparison of a failed guard: `field op wants`, and the value the kernel read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardCondition {
    /// The state variable or field compared (`status` for the lifecycle state).
    pub field: String,
    /// The operator as written in the guard: `==`, `!=`, `<`, `<=`, `>`, `>=`,
    /// `in` or `not in`. A bare boolean `x` is `==` with `wants` true.
    pub op: String,
    /// The value the guard wants, typed as written (a list for `in`).
    pub wants: Value,
    /// The value read, typed as stored; `null` when absent.
    pub value: Value,
}

/// An action input refused by its declared parameter contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError {
    /// The message callers read.
    pub message: String,
    /// The typed reason, when the refusal names a parameter's constraint.
    pub refusal: Option<Refusal>,
}

impl ContractError {
    /// A refusal with only a message.
    pub fn message(message: String) -> Self {
        Self {
            message,
            refusal: None,
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ContractError> for String {
    fn from(error: ContractError) -> Self {
        error.message
    }
}
