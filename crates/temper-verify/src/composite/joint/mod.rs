//! Invariants over several entities.
//!
//! An `[[invariant]]` may read a related entity: its status
//! (`Type[id].status`) or a field (`Type[id].field`). One entity's model
//! cannot know those values, so the per-entity cascade reads them as unknown
//! and never fails on them. They are checked here, on the joint state of the
//! invariant's entity and every type it names, one instance of each (the
//! records an id field points at):
//!
//! - every action of every entity in scope is offered, as in the per-entity
//!   model, with its guard resolved against the joint state;
//! - an entity reaction between two types in scope fires inside the step
//!   that caused it, and carries its `args`: a tracked string copied from
//!   the source reaches the target's variable of the parameter's name;
//! - string variables are tracked as symbols (equality only) when an
//!   invariant reads them, or when their value reaches one that is read
//!   ([`crate::model::build_model_tracking`]);
//! - each entity is sliced to what the invariants depend on: a counter,
//!   boolean or list nothing read can be influenced by is dropped, and a
//!   guard that read it becomes a free choice. That only adds behaviour, so
//!   an invariant that holds here holds in the full model; a broken one
//!   comes with the path that breaks it, to read against the spec.
//!
//! The walk is breadth first, so each broken invariant's path is a shortest
//! one. A walk that runs out of its [`Budget`] says so and is never a pass.

mod env;
mod model;
mod slice;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Truth, eval};

use crate::Budget;
use crate::model::semantics::string_values;
use crate::model::{ParamValue, TemperModelAction, TemperModelState};

use model::JointModel;

/// One entity type's state per type in scope.
pub type JointState = BTreeMap<String, TemperModelState>;

/// What the joint check found for one invariant.
#[derive(Debug, Clone)]
pub struct JointInvariantResult {
    /// The entity type that declares the invariant.
    pub entity: String,
    /// The invariant's name.
    pub invariant: String,
    /// Its assertion, as written.
    pub assert: String,
    /// The entity types composed to check it.
    pub scope: Vec<String>,
    /// Distinct joint states walked.
    pub states: usize,
    /// Holds, broken (with a shortest path), or not fully checked.
    pub outcome: JointOutcome,
}

impl JointInvariantResult {
    /// Whether the invariant was checked over every reachable joint state and
    /// held in all of them.
    pub fn holds(&self) -> bool {
        matches!(self.outcome, JointOutcome::Holds)
    }
}

/// The outcome of checking one invariant on the joint model.
#[derive(Debug, Clone)]
pub enum JointOutcome {
    /// It held in every reachable joint state.
    Holds,
    /// A reachable joint state breaks it. `path` starts at the initial joint
    /// state; each later step names the move that led to it.
    Broken {
        /// The moves and the joint states they reach.
        path: Vec<JointStep>,
    },
    /// The walk stopped before every state was seen, or could not start.
    Incomplete {
        /// Why.
        reason: String,
    },
}

/// One step of a counterexample path.
#[derive(Debug, Clone)]
pub struct JointStep {
    /// The move, `Entity.Action(params) -> Status`; `None` for the start.
    pub action: Option<String>,
    /// The joint state after it.
    pub state: String,
}

impl fmt::Display for JointInvariantResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = self.scope.join(" + ");
        match &self.outcome {
            JointOutcome::Holds => write!(
                f,
                "{}.{} HOLDS over {} joint states ({scope})",
                self.entity, self.invariant, self.states
            ),
            JointOutcome::Broken { path } => {
                writeln!(
                    f,
                    "{}.{} BROKEN after {} moves ({scope}, {} joint states walked): {}",
                    self.entity,
                    self.invariant,
                    path.len().saturating_sub(1),
                    self.states,
                    self.assert
                )?;
                for step in path {
                    match &step.action {
                        Some(action) => writeln!(f, "  {action}\n      {}", step.state)?,
                        None => writeln!(f, "  start\n      {}", step.state)?,
                    }
                }
                Ok(())
            }
            JointOutcome::Incomplete { reason } => write!(
                f,
                "{}.{} NOT CHECKED ({scope}, {} joint states walked): {reason}",
                self.entity, self.invariant, self.states
            ),
        }
    }
}

/// Check every `[[invariant]]` in `automatons` that reads a related entity,
/// on the joint model of its entity and the types it names.
pub fn verify_joint_invariants(
    automatons: &[&Automaton],
    budget: Budget,
) -> Vec<JointInvariantResult> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut results = Vec::new();
    for (name, automaton) in &by_name {
        let joint: Vec<&temper_spec::automaton::Invariant> = automaton
            .invariants
            .iter()
            .filter(|inv| !inv.assert.related_types().is_empty())
            .collect();
        if joint.is_empty() {
            continue;
        }
        let mut scope: BTreeSet<String> = BTreeSet::from([name.to_string()]);
        let mut missing = Vec::new();
        for inv in &joint {
            for (entity_type, _) in inv.assert.related_types() {
                if by_name.contains_key(entity_type.as_str()) {
                    scope.insert(entity_type);
                } else {
                    missing.push(entity_type);
                }
            }
        }
        let result =
            |inv: &temper_spec::automaton::Invariant, scope: &BTreeSet<String>, states, outcome| {
                JointInvariantResult {
                    entity: name.to_string(),
                    invariant: inv.name.clone(),
                    assert: inv.assert.to_string(),
                    scope: scope.iter().cloned().collect(),
                    states,
                    outcome,
                }
            };
        if !missing.is_empty() {
            for inv in &joint {
                let reason = format!(
                    "it reads {}, which this app does not declare",
                    missing.join(", ")
                );
                results.push(result(inv, &scope, 0, JointOutcome::Incomplete { reason }));
            }
            continue;
        }
        let model = JointModel::new(&by_name, scope, name, &joint);
        let exprs: Vec<&Expr> = joint.iter().map(|inv| &inv.assert).collect();
        let (states, outcomes) = model.walk(name, &exprs, budget);
        for (inv, outcome) in joint.iter().zip(outcomes) {
            results.push(result(inv, &model.scope, states, outcome));
        }
    }
    results
}

/// One move of a trace: an entity's action with the string parameters it
/// was sent. Values are names for this trace only: equal names are equal
/// values, `''` is the empty string, and a name not seen before is a value
/// nothing holds yet. A parameter the model does not track is not checked.
#[derive(Debug, Clone)]
pub struct TraceMove {
    /// The entity type.
    pub entity: String,
    /// The action.
    pub action: String,
    /// String parameters by name; parameters left out may take any value.
    pub params: BTreeMap<String, String>,
}

/// A trace walked through the joint model of `entity`'s cross-entity
/// invariants.
#[derive(Debug, Clone)]
pub struct JointReplay {
    /// The start and the state after each move.
    pub steps: Vec<JointStep>,
    /// The invariants false in the last state.
    pub broken: Vec<String>,
}

/// Walk `trace` through the joint model of `entity`'s invariants that read
/// related entities, as the check does: whether the spec allows each move,
/// and which invariants the last state breaks. A reaction whose guard the
/// model cannot decide is taken as not firing. Use it to ask whether what
/// happened in production is a path of the spec.
pub fn replay_joint_invariants(
    automatons: &[&Automaton],
    entity: &str,
    trace: &[TraceMove],
) -> Result<JointReplay, String> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let automaton = by_name
        .get(entity)
        .ok_or_else(|| format!("no entity type {entity}"))?;
    let joint: Vec<&temper_spec::automaton::Invariant> = automaton
        .invariants
        .iter()
        .filter(|inv| !inv.assert.related_types().is_empty())
        .collect();
    let mut scope = BTreeSet::from([entity.to_string()]);
    for inv in &joint {
        for (entity_type, _) in inv.assert.related_types() {
            if !by_name.contains_key(entity_type.as_str()) {
                return Err(format!(
                    "{entity} reads {entity_type}, which is not declared"
                ));
            }
            scope.insert(entity_type);
        }
    }
    let model = JointModel::new(&by_name, scope, entity, &joint);
    let mut state = model.init();
    let mut steps = vec![JointStep {
        action: None,
        state: render(&state),
    }];
    // Each trace name's value in the current state.
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for (i, wanted) in trace.iter().enumerate() {
        let held = string_values(state.values());
        let fresh = format!(
            "#{}",
            held.iter().filter(|v| v.starts_with('#')).count() + 1
        );
        let fits = |action: &TemperModelAction| {
            action.name == wanted.action
                && wanted.params.iter().all(|(param, name)| {
                    // A parameter the model does not track takes any value.
                    let Some(ParamValue::Str(value)) = action.params.get(param) else {
                        return true;
                    };
                    match names.get(name) {
                        Some(bound) => value == bound,
                        None if name.is_empty() => value.is_empty(),
                        None => *value == fresh,
                    }
                })
        };
        let Some((_, action)) = model
            .moves(&state)
            .into_iter()
            .find(|(e, action)| e == &wanted.entity && fits(action))
        else {
            return Err(format!(
                "move {} ({}.{}) is not allowed in {}",
                i + 1,
                wanted.entity,
                wanted.action,
                render(&state)
            ));
        };
        for (param, name) in &wanted.params {
            if let Some(ParamValue::Str(value)) = action.params.get(param)
                && !name.is_empty()
            {
                names.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        let (next, renamed) = model
            .next(&state, &wanted.entity, &action)
            .into_iter()
            .next()
            .ok_or_else(|| {
                format!(
                    "move {} ({}.{}) cannot be taken",
                    i + 1,
                    wanted.entity,
                    wanted.action
                )
            })?;
        names = names
            .into_iter()
            .filter_map(|(name, value)| match renamed.get(&value) {
                Some(new) => Some((name, new.clone())),
                None if !value.starts_with('#') => Some((name, value)),
                None => None,
            })
            .collect();
        steps.push(JointStep {
            action: Some(label(&wanted.entity, &action, &renamed)),
            state: render(&next),
        });
        state = next;
    }
    let env = model.env(entity, &state);
    let broken = joint
        .iter()
        .filter(|inv| eval(&inv.assert, &env) == Truth::False)
        .map(|inv| inv.name.clone())
        .collect();
    Ok(JointReplay { steps, broken })
}

/// The moves from the start to `nodes[index]`.
pub(super) fn path_to(
    nodes: &[(JointState, Option<(usize, String)>)],
    mut index: usize,
) -> Vec<JointStep> {
    let mut steps = Vec::new();
    loop {
        let (state, parent) = &nodes[index];
        steps.push(JointStep {
            action: parent.as_ref().map(|(_, action)| action.clone()),
            state: render(state),
        });
        match parent {
            Some((from, _)) => index = *from,
            None => break,
        }
    }
    steps.reverse();
    steps
}

pub(super) fn render(state: &JointState) -> String {
    state
        .iter()
        .map(|(entity, s)| format!("{entity}={s}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `entity.action`, its string parameters named as in the state it reached.
pub(super) fn label(
    entity: &str,
    action: &TemperModelAction,
    renamed: &BTreeMap<String, String>,
) -> String {
    let mut action = action.clone();
    for value in action.params.values_mut() {
        if let ParamValue::Str(text) = value
            && let Some(name) = renamed.get(text)
        {
            *text = name.clone();
        }
    }
    format!("{entity}.{action}")
}

#[cfg(test)]
#[path = "joint_test.rs"]
mod tests;
