//! The joint model: the entities in scope, their reactions, and the walk.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use temper_spec::automaton::{Automaton, TriggerKind};
use temper_spec::predicate::{Arg, Expr, Literal, Name, Truth, eval};

use super::env::JointEnv;
use super::slice::slice;
use super::{JointOutcome, JointState, label, path_to};
use crate::Budget;
use crate::model::semantics::{canonical_strings, step, string_values, successors_among};
use crate::model::{
    ParamValue, ResolvedTransition, StringParam, TemperModel, TemperModelAction, TemperModelState,
    build_model_tracking,
};

/// Counter ceiling for the joint models, as for the composite plans.
const JOINT_MAX_COUNTER: usize = 3;

/// Reaction depth followed inside one step: the runtime's own bound.
const MAX_REACTION_DEPTH: u32 = temper_runtime::reaction::MAX_REACTION_DEPTH;

/// An entity reaction between two types in scope, with the arguments it
/// passes (target parameter to a source field or a literal).
#[derive(Debug, Clone)]
struct Reaction {
    from: String,
    source_action: String,
    to: String,
    target_action: String,
    guard: Option<Expr>,
    args: BTreeMap<String, Arg>,
}

/// The joint model of one entity's invariants that read related entities.
pub(super) struct JointModel {
    models: BTreeMap<String, TemperModel>,
    reactions: Vec<Reaction>,
    /// The types composed: those the invariants name, and those a
    /// `param_equals_related_field` of a tracked parameter reads.
    pub(super) scope: BTreeSet<String>,
}

impl JointModel {
    pub(super) fn new(
        by_name: &BTreeMap<&str, &Automaton>,
        mut scope: BTreeSet<String>,
        owner: &str,
        invariants: &[&temper_spec::automaton::Invariant],
    ) -> Self {
        // What the invariants read: the owner's variables, and the related
        // types' fields.
        let mut read: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for inv in invariants {
            inv.assert.for_each_name(&mut |n| match n {
                Name::Var(var) => {
                    read.entry(owner.to_string())
                        .or_default()
                        .insert(var.to_string());
                }
                Name::CrossField {
                    entity_type, field, ..
                } => {
                    read.entry(entity_type.to_string())
                        .or_default()
                        .insert(field.to_string());
                }
                Name::Status | Name::CrossStatus { .. } => {}
            });
        }
        // Tracked strings: what is read, what a reaction copies into a
        // tracked variable, and the related field a tracked parameter must
        // equal (whose type joins the scope), until nothing more reaches one.
        let mut also: BTreeMap<String, BTreeSet<String>> = read.clone();
        let (models, reactions) = loop {
            let reactions = reactions_in(by_name, &scope);
            let models: BTreeMap<String, TemperModel> = scope
                .iter()
                .map(|entity| {
                    let extra = also.get(entity).cloned().unwrap_or_default();
                    (
                        entity.clone(),
                        build_model_tracking(by_name[entity.as_str()], JOINT_MAX_COUNTER, &extra),
                    )
                })
                .collect();
            let mut grew = false;
            for model in models.values() {
                for t in &model.transitions {
                    for (entity, field) in t
                        .string_params
                        .iter()
                        .filter_map(|p| p.equals_related.as_ref())
                    {
                        if by_name.contains_key(entity.as_str()) {
                            grew |= scope.insert(entity.clone());
                            grew |= also
                                .entry(entity.clone())
                                .or_default()
                                .insert(field.clone());
                        }
                    }
                }
            }
            for reaction in &reactions {
                for (param, arg) in &reaction.args {
                    if let Arg::Var(field) = arg
                        && models[&reaction.to].tracked_strings.contains(param)
                        && !models[&reaction.from].tracked_strings.contains(field)
                    {
                        grew |= also
                            .entry(reaction.from.clone())
                            .or_default()
                            .insert(field.clone());
                    }
                }
            }
            if !grew {
                break (models, reactions);
            }
        };
        let models = models
            .into_iter()
            .map(|(entity, model)| {
                let keep = read.get(&entity).cloned().unwrap_or_default();
                (entity, slice(model, keep))
            })
            .collect();
        Self {
            models,
            reactions,
            scope,
        }
    }

    pub(super) fn init(&self) -> JointState {
        let mut state: JointState = self
            .models
            .iter()
            .map(|(entity, model)| {
                let init = stateright::Model::init_states(model)
                    .into_iter()
                    .next()
                    .expect("a model has one initial state");
                (entity.clone(), init)
            })
            .collect();
        canonical(&mut state);
        state
    }

    /// Every move from `state`: each entity's enabled actions, with string
    /// parameters chosen among the values held anywhere in the joint state.
    pub(super) fn moves(&self, state: &JointState) -> Vec<(String, TemperModelAction)> {
        let values = string_values(state.values());
        let mut out = Vec::new();
        for (entity, model) in &self.models {
            let local = &state[entity];
            for t in &model.transitions {
                if !self.enabled(entity, t, state) {
                    continue;
                }
                for (params, _) in successors_among(model, t, local, &values) {
                    if !self.related_hold(t, &params, state) {
                        continue;
                    }
                    out.push((
                        entity.clone(),
                        TemperModelAction {
                            name: t.name.clone(),
                            target_state: t.to_state.clone(),
                            params,
                        },
                    ));
                }
            }
        }
        out
    }

    /// Whether `params` meets `t`'s `param_equals_related_field` constraints
    /// on `state`, as the runtime checks them when the action is dispatched.
    /// A related type outside the scope, or a field not tracked, leaves the
    /// parameter free.
    fn related_hold(
        &self,
        t: &ResolvedTransition,
        params: &BTreeMap<String, ParamValue>,
        state: &JointState,
    ) -> bool {
        t.string_params.iter().all(|param| {
            let Some((entity, field)) = &param.equals_related else {
                return true;
            };
            let Some(related) = state.get(entity) else {
                return true;
            };
            if !self.models[entity].tracked_strings.contains(field) {
                return true;
            }
            match params.get(&param.name) {
                Some(ParamValue::Str(value)) => {
                    related.strings.get(field).is_some_and(|held| held == value)
                }
                _ => false,
            }
        })
    }

    fn enabled(&self, entity: &str, t: &ResolvedTransition, state: &JointState) -> bool {
        let local = &state[entity];
        let status_ok =
            t.from_states.is_empty() || t.from_states.iter().any(|s| s == &local.status);
        status_ok && eval(&t.guard, &self.env(entity, state)).may_hold()
    }

    pub(super) fn env<'a>(&'a self, owner: &'a str, state: &'a JointState) -> JointEnv<'a> {
        JointEnv {
            models: &self.models,
            state,
            owner,
        }
    }

    /// The states after `entity` takes `action`, with the reactions it
    /// causes: one per way the reactions' guards can go, the ones where a
    /// guard the model cannot decide does not fire first. Each comes with
    /// the new names of the values `state` held.
    pub(super) fn next(
        &self,
        state: &JointState,
        entity: &str,
        action: &TemperModelAction,
    ) -> Vec<(JointState, BTreeMap<String, String>)> {
        let model = &self.models[entity];
        let Some(moved) = step(
            model,
            &action.name,
            action.target_state.as_deref(),
            &state[entity],
            &action.params,
        ) else {
            return Vec::new();
        };
        let mut next = state.clone();
        next.insert(entity.to_string(), moved);
        self.react(next, entity, &action.name, 0)
            .into_iter()
            .map(|mut outcome| {
                let renamed = canonical(&mut outcome);
                (outcome, renamed)
            })
            .collect()
    }

    /// Fire the reactions of `source.action` between types in scope, in
    /// declaration order, as the runtime does after the source commits. A
    /// reaction fires when its guard holds on the source; a guard that reads
    /// what the model does not track may go either way, so both outcomes are
    /// kept. A reaction whose target cannot take it is dropped (the ADR-0150
    /// check reports those).
    fn react(&self, state: JointState, source: &str, action: &str, depth: u32) -> Vec<JointState> {
        let mut outcomes = vec![state];
        if depth >= MAX_REACTION_DEPTH {
            return outcomes;
        }
        for reaction in self
            .reactions
            .iter()
            .filter(|r| r.from == source && r.source_action == action)
        {
            let mut after = Vec::new();
            for state in outcomes {
                let fires = match &reaction.guard {
                    Some(guard) => eval(guard, &self.env(source, &state)),
                    None => Truth::True,
                };
                if fires != Truth::True {
                    after.push(state.clone());
                }
                if fires == Truth::False {
                    continue;
                }
                match self.deliver(&state, source, reaction) {
                    Some((moved, name)) => {
                        let mut next = state;
                        next.insert(reaction.to.clone(), moved);
                        after.extend(self.react(next, &reaction.to, &name, depth + 1));
                    }
                    None => after.push(state),
                }
            }
            outcomes = after;
        }
        outcomes
    }

    /// The target's state after it takes `reaction` from `source`, and the
    /// action it took; `None` when it cannot take it.
    fn deliver(
        &self,
        state: &JointState,
        source: &str,
        reaction: &Reaction,
    ) -> Option<(TemperModelState, String)> {
        let source_state = &state[source];
        let model = &self.models[&reaction.to];
        let t = model
            .transitions
            .iter()
            .find(|t| t.name == reaction.target_action && self.enabled(&reaction.to, t, state))?;
        let target = &state[&reaction.to];
        let values = string_values(state.values());
        // Counts and booleans an effect reads: the first choice, as the
        // composite check takes. Strings: what the reaction passes, and a
        // parameter it does not pass leaves the field as it is.
        let mut params: BTreeMap<String, ParamValue> = successors_among(model, t, target, &values)
            .into_iter()
            .next()
            .map(|(params, _)| params)
            .unwrap_or_default();
        for param in &t.string_params {
            params.remove(&param.name);
            let passed = match reaction.args.get(&param.name) {
                Some(Arg::Var(field)) => source_state.strings.get(field).cloned(),
                Some(Arg::Lit(Literal::Str(text))) => Some(text.clone()),
                Some(Arg::Lit(Literal::Null)) => Some(String::new()),
                _ => None,
            };
            if let Some(value) = passed {
                params.insert(param.name.clone(), ParamValue::Str(value));
            }
        }
        if !constraints_hold(&t.string_params, target, &params)
            || !self.related_hold(t, &params, state)
        {
            return None;
        }
        let moved = step(model, &t.name, t.to_state.as_deref(), target, &params)?;
        Some((moved, t.name.clone()))
    }

    /// Walk every reachable joint state breadth first, checking `invariants`
    /// of `owner` in each. Returns the states walked and one outcome per
    /// invariant.
    pub(super) fn walk(
        &self,
        owner: &str,
        invariants: &[&Expr],
        budget: Budget,
    ) -> (usize, Vec<JointOutcome>) {
        let started = Instant::now();
        let mut nodes: Vec<(JointState, Option<(usize, String)>)> = Vec::new();
        let mut seen: HashMap<JointState, usize> = HashMap::new();
        let mut queue = VecDeque::new();
        let init = self.init();
        seen.insert(init.clone(), 0);
        nodes.push((init, None));
        queue.push_back(0usize);
        let mut broken: Vec<Option<usize>> = vec![None; invariants.len()];
        let mut stopped = None;
        while let Some(index) = queue.pop_front() {
            let state = nodes[index].0.clone();
            {
                let env = self.env(owner, &state);
                for (i, inv) in invariants.iter().enumerate() {
                    if broken[i].is_none() && eval(inv, &env) == Truth::False {
                        broken[i] = Some(index);
                    }
                }
            }
            if broken.iter().all(Option::is_some) {
                break;
            }
            if nodes.len() >= budget.states || started.elapsed() >= budget.time {
                stopped = Some(if nodes.len() >= budget.states {
                    format!("the walk reached its {} states", budget.states)
                } else {
                    format!("the walk ran out of its {} s", budget.time.as_secs())
                });
                break;
            }
            for (entity, action) in self.moves(&state) {
                for (next, renamed) in self.next(&state, &entity, &action) {
                    if seen.contains_key(&next) {
                        continue;
                    }
                    let at = nodes.len();
                    seen.insert(next.clone(), at);
                    nodes.push((next, Some((index, label(&entity, &action, &renamed)))));
                    queue.push_back(at);
                }
            }
        }
        let outcomes = broken
            .iter()
            .map(|found| match found {
                Some(index) => JointOutcome::Broken {
                    path: path_to(&nodes, *index),
                },
                None => match &stopped {
                    Some(reason) => JointOutcome::Incomplete {
                        reason: reason.clone(),
                    },
                    None => JointOutcome::Holds,
                },
            })
            .collect();
        (nodes.len(), outcomes)
    }
}

/// Renumber new strings over the whole joint state; returns each value's new
/// name.
fn canonical(state: &mut JointState) -> BTreeMap<String, String> {
    let mut maps: Vec<&mut BTreeMap<String, String>> =
        state.values_mut().map(|s| &mut s.strings).collect();
    canonical_strings(&mut maps)
}

/// Whether the string parameters `params` passes satisfy the action's
/// constraints, which the runtime checks on every dispatch.
fn constraints_hold(
    string_params: &[StringParam],
    target: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> bool {
    string_params.iter().all(|param| {
        // A string parameter nobody passed is absent, which only a
        // non-empty constraint refuses.
        let Some(ParamValue::Str(value)) = params.get(&param.name) else {
            return !param.nonempty;
        };
        let tracked = |var: &String| target.strings.get(var);
        !(param.nonempty && value.is_empty())
            && param
                .equals
                .as_ref()
                .and_then(tracked)
                .is_none_or(|v| v == value)
            && param
                .not_equals
                .iter()
                .filter_map(tracked)
                .all(|v| v != value)
    })
}

/// The entity reactions whose source and target are both in `scope`.
fn reactions_in(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> Vec<Reaction> {
    let mut reactions = Vec::new();
    for from in scope {
        for action in &by_name[from.as_str()].actions {
            for trigger in &action.triggers {
                if trigger.kind != TriggerKind::Entity {
                    continue;
                }
                let (Some(to), Some(target_action)) =
                    (&trigger.target_entity, &trigger.target_action)
                else {
                    continue;
                };
                if !scope.contains(to) {
                    continue;
                }
                reactions.push(Reaction {
                    from: from.clone(),
                    source_action: action.name.clone(),
                    to: to.clone(),
                    target_action: target_action.clone(),
                    guard: trigger.guard.clone(),
                    args: trigger.args.clone(),
                });
            }
        }
    }
    reactions
}
