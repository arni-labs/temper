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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::time::Instant;

use temper_spec::automaton::{Automaton, TriggerKind};
use temper_spec::predicate::{Arg, Env, Expr, Literal, Name, Truth, Val, VarKind, eval};

use crate::Budget;
use crate::model::builder::effect_params;
use crate::model::semantics::{canonical_strings, step, string_values, successors_among};
use crate::model::{
    ModelEffect, ParamValue, ResolvedTransition, StringParam, TemperModel, TemperModelAction,
    TemperModelState, build_model_tracking,
};

/// Counter ceiling for the joint models, as for the composite plans.
const JOINT_MAX_COUNTER: usize = 3;

/// Reaction depth followed inside one step: the runtime's own bound.
const MAX_REACTION_DEPTH: u32 = temper_runtime::reaction::MAX_REACTION_DEPTH;

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
struct JointModel {
    models: BTreeMap<String, TemperModel>,
    reactions: Vec<Reaction>,
    /// The types composed: those the invariants name, and those a
    /// `param_equals_related_field` of a tracked parameter reads.
    scope: BTreeSet<String>,
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

impl JointModel {
    fn new(
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

    fn init(&self) -> JointState {
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
    fn moves(&self, state: &JointState) -> Vec<(String, TemperModelAction)> {
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

    fn env<'a>(&'a self, owner: &'a str, state: &'a JointState) -> JointEnv<'a> {
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
    fn next(
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
    fn walk(
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

/// The moves from the start to `nodes[index]`.
fn path_to(nodes: &[(JointState, Option<(usize, String)>)], mut index: usize) -> Vec<JointStep> {
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

fn render(state: &JointState) -> String {
    state
        .iter()
        .map(|(entity, s)| format!("{entity}={s}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Renumber new strings over the whole joint state; returns each value's new
/// name.
fn canonical(state: &mut JointState) -> BTreeMap<String, String> {
    let mut maps: Vec<&mut BTreeMap<String, String>> =
        state.values_mut().map(|s| &mut s.strings).collect();
    canonical_strings(&mut maps)
}

/// `entity.action`, its string parameters named as in the state it reached.
fn label(entity: &str, action: &TemperModelAction, renamed: &BTreeMap<String, String>) -> String {
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

/// `model` with only the counters, booleans and lists `read` names, or that
/// can change one that is kept: a transition that writes a kept variable
/// keeps what its guard and effects read. Everything else is dropped, so a
/// guard that read it is a free choice and its effects are gone. Tracked
/// strings and the status are always kept.
fn slice(mut model: TemperModel, read: BTreeSet<String>) -> TemperModel {
    let mut keep: BTreeSet<String> = read;
    keep.extend(model.tracked_strings.iter().cloned());
    loop {
        let before = keep.len();
        for t in &model.transitions {
            let writes = t.effects.iter().any(|e| keep.contains(effect_var(e)))
                || t.string_params.iter().any(|p| keep.contains(&p.name));
            if !writes {
                continue;
            }
            t.guard.for_each_name(&mut |n| {
                if let Name::Var(var) = n {
                    keep.insert(var.to_string());
                }
            });
            for effect in &t.effects {
                if let Some(var) = effect_reads(effect) {
                    keep.insert(var.to_string());
                }
            }
        }
        if keep.len() == before {
            break;
        }
    }
    let modeled = |kind: &VarKind| matches!(kind, VarKind::Counter | VarKind::Bool | VarKind::List);
    let dropped: BTreeSet<String> = model
        .var_kinds
        .iter()
        .filter(|(name, kind)| modeled(kind) && !keep.contains(*name))
        .map(|(name, _)| name.clone())
        .collect();
    for name in &dropped {
        model.var_kinds.remove(name);
        model.initial_counters.remove(name);
        model.initial_booleans.remove(name);
        model.initial_lists.remove(name);
        model.counter_bounds.remove(name);
        model.list_literals.remove(name);
    }
    model.invariants.clear();
    model.liveness.clear();
    for t in &mut model.transitions {
        t.effects.retain(|e| !dropped.contains(effect_var(e)));
        t.params = effect_params(&t.effects);
    }
    model
}

fn effect_var(effect: &ModelEffect) -> &str {
    match effect {
        ModelEffect::SetCounter { var, .. }
        | ModelEffect::AddCounter { var, .. }
        | ModelEffect::SubCounter { var, .. }
        | ModelEffect::SetBool { var, .. }
        | ModelEffect::ListAppend { var, .. }
        | ModelEffect::ListRemoveAt { var, .. } => var,
    }
}

fn effect_reads(effect: &ModelEffect) -> Option<&str> {
    let arg = match effect {
        ModelEffect::SetCounter { value, .. }
        | ModelEffect::AddCounter { value, .. }
        | ModelEffect::SubCounter { value, .. }
        | ModelEffect::SetBool { value, .. }
        | ModelEffect::ListAppend { value, .. } => value,
        ModelEffect::ListRemoveAt { index, .. } => index,
    };
    match arg {
        Arg::Var(var) => Some(var),
        _ => None,
    }
}

/// One entity of a joint state as a predicate environment: its own
/// variables as the per-entity model reads them, and the related entities'
/// statuses and tracked variables from the joint state.
struct JointEnv<'a> {
    models: &'a BTreeMap<String, TemperModel>,
    state: &'a JointState,
    owner: &'a str,
}

fn var_of<'a>(model: &TemperModel, state: &'a TemperModelState, name: &str) -> Val<'a> {
    match model.var_kinds.get(name) {
        Some(VarKind::Counter) => Val::Num(state.counters.get(name).copied().unwrap_or(0) as f64),
        Some(VarKind::Bool) => Val::Bool(state.booleans.get(name).copied().unwrap_or(false)),
        Some(VarKind::List) => state
            .lists
            .get(name)
            .map_or(Val::Strs(&[]), |items| Val::Strs(items)),
        Some(VarKind::Str) => state
            .strings
            .get(name)
            .map_or(Val::Unknown, |value| Val::Str(value)),
        Some(VarKind::Num) | None => Val::Unknown,
    }
}

impl Env for JointEnv<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(&self.state[self.owner].status)
    }

    fn var(&self, name: &str) -> Val<'_> {
        var_of(&self.models[self.owner], &self.state[self.owner], name)
    }

    fn cross_statuses(&self, entity_type: &str, _id_field: &str) -> Option<Vec<Val<'_>>> {
        self.state
            .get(entity_type)
            .map(|s| vec![Val::Str(&s.status)])
    }

    fn cross_fields(
        &self,
        entity_type: &str,
        _id_field: &str,
        field: &str,
    ) -> Option<Vec<Val<'_>>> {
        let state = self.state.get(entity_type)?;
        let model = self.models.get(entity_type)?;
        Some(vec![var_of(model, state, field)])
    }
}
