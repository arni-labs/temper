//! The joint model: the records in scope, their reactions, the dispatches
//! on their way between them, and the walk.

mod step;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};
use temper_spec::predicate::{Arg, Expr, Name, Truth, eval};

use super::env::{Blind, ID, JointEnv};
use super::slice::slice;
use super::{JointOutcome, JointState, path_to};
use crate::Budget;
use crate::model::semantics::{canonical_strings, string_values};
use crate::model::{
    ParamValue, ResolvedTransition, TemperModel, TemperModelState, build_model_tracking,
};

pub(super) use step::Move;

/// Counter ceiling for the joint models, as for the composite plans.
const JOINT_MAX_COUNTER: usize = 3;

/// Reaction depth followed: the runtime's own bound.
const MAX_REACTION_DEPTH: u32 = temper_runtime::reaction::MAX_REACTION_DEPTH;

/// Dispatches in flight at once. The runtime has no such bound: each request
/// and each reaction is its own task. The model needs one to stay finite;
/// three lets a late dispatch land after two later ones. A walk that held a
/// move back for it says so, and then proves only up to this many.
pub(super) const MAX_IN_FLIGHT: usize = 3;

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
    resolve: Option<TargetResolver>,
}

/// What a walk found.
pub(super) struct Walk {
    /// Distinct joint states walked.
    pub(super) states: usize,
    /// One per invariant.
    pub(super) outcomes: Vec<JointOutcome>,
    /// Whether a move was held back for [`MAX_IN_FLIGHT`].
    pub(super) held_back: bool,
}

/// The joint model of one entity's invariants that read related entities.
pub(super) struct JointModel {
    models: BTreeMap<String, TemperModel>,
    reactions: Vec<Reaction>,
    /// The types composed: those the invariants name.
    pub(super) scope: BTreeSet<String>,
}

impl JointModel {
    pub(super) fn new(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: BTreeSet<String>,
        owner: &str,
        invariants: &[&temper_spec::automaton::Invariant],
    ) -> Self {
        // What the invariants read: the owner's variables and the id fields
        // its references use, and the related types' fields.
        let mut read: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut owned = |entity: &str, field: &str| {
            if !matches!(field, "Id" | "id") {
                read.entry(entity.to_string())
                    .or_default()
                    .insert(field.to_string());
            }
        };
        for inv in invariants {
            inv.assert.for_each_name(&mut |n| match n {
                Name::Var(var) => owned(owner, var),
                Name::CrossStatus { id_field, .. } => owned(owner, id_field),
                Name::CrossField {
                    entity_type,
                    id_field,
                    field,
                } => {
                    owned(owner, id_field);
                    owned(entity_type, field);
                }
                Name::Status => {}
            });
        }
        // Tracked strings: what is read, the id fields references use, and
        // what a reaction copies into a tracked variable, until nothing more
        // reaches one.
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
            let mut track = |entity: &str, field: &str| {
                !matches!(field, "Id" | "id")
                    && also
                        .entry(entity.to_string())
                        .or_default()
                        .insert(field.to_string())
            };
            for (entity, model) in &models {
                for t in &model.transitions {
                    for (related, id_field) in t.guard.related_types() {
                        if scope.contains(&related) {
                            grew |= track(entity, &id_field);
                        }
                    }
                }
            }
            for reaction in &reactions {
                if let Some(TargetResolver::Field { id_field }) = &reaction.resolve {
                    grew |= track(&reaction.from, id_field);
                }
                for (param, arg) in &reaction.args {
                    if let Arg::Var(field) = arg
                        && models[&reaction.to].tracked_strings.contains(param)
                        && !models[&reaction.from].tracked_strings.contains(field)
                    {
                        grew |= track(&reaction.from, field);
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

    /// The records as they start, without their ids.
    fn blank(&self) -> BTreeMap<String, TemperModelState> {
        self.models
            .iter()
            .map(|(entity, model)| {
                let init = stateright::Model::init_states(model)
                    .into_iter()
                    .next()
                    .expect("a model has one initial state");
                (entity.clone(), init)
            })
            .collect()
    }

    /// The starting states: one per way the records' ids can be equal or
    /// differ.
    fn inits(&self) -> Vec<JointState> {
        let blank = self.blank();
        partitions(blank.len())
            .into_iter()
            .map(|blocks| {
                let mut records = blank.clone();
                for (record, block) in records.values_mut().zip(blocks) {
                    record
                        .strings
                        .insert(ID.to_string(), format!("#{}", block + 1));
                }
                let mut state = JointState {
                    records,
                    inflight: Vec::new(),
                };
                canonical(&mut state);
                state
            })
            .collect()
    }

    /// The starting state whose ids are `ids` (entity type to a trace name;
    /// equal names, equal ids; a type left out gets an id of its own), and
    /// each trace name's value in it.
    pub(super) fn init_with_ids(
        &self,
        ids: &BTreeMap<String, String>,
    ) -> (JointState, BTreeMap<String, String>) {
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        let mut records = self.blank();
        for (entity, record) in &mut records {
            let name = ids
                .get(entity)
                .cloned()
                .unwrap_or_else(|| format!("{entity}#id"));
            let next = format!("#{}", names.len() + 1);
            let value = names.entry(name).or_insert(next).clone();
            record.strings.insert(ID.to_string(), value);
        }
        let mut state = JointState {
            records,
            inflight: Vec::new(),
        };
        let renamed = canonical(&mut state);
        for value in names.values_mut() {
            if let Some(new) = renamed.get(value) {
                *value = new.clone();
            }
        }
        (state, names)
    }

    /// The strings `state` holds, in records and in dispatches.
    pub(super) fn values(&self, state: &JointState) -> BTreeSet<String> {
        let mut values = string_values(state.records.values());
        for dispatch in &state.inflight {
            for value in dispatch.params.values() {
                if let ParamValue::Str(text) = value
                    && !text.is_empty()
                {
                    values.insert(text.clone());
                }
            }
        }
        values
    }

    /// Whether `t`'s guard reads a related record in scope.
    fn reads_related(&self, t: &ResolvedTransition) -> bool {
        t.guard
            .related_types()
            .iter()
            .any(|(entity, _)| self.models.contains_key(entity))
    }

    /// Whether some transition named `action` of `entity` reads a related
    /// record.
    fn action_reads_related(&self, entity: &str, action: &str) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .any(|t| t.name == action && self.reads_related(t))
    }

    fn enabled(
        &self,
        entity: &str,
        t: &ResolvedTransition,
        state: &JointState,
        blind: Blind,
    ) -> bool {
        let local = &state.records[entity];
        let status_ok =
            t.from_states.is_empty() || t.from_states.iter().any(|s| s == &local.status);
        status_ok && eval(&t.guard, &self.env_blind(entity, state, blind)).may_hold()
    }

    pub(super) fn env<'a>(&'a self, owner: &'a str, state: &'a JointState) -> JointEnv<'a> {
        self.env_blind(owner, state, Blind::Nothing)
    }

    fn env_blind<'a>(
        &'a self,
        owner: &'a str,
        state: &'a JointState,
        blind: Blind,
    ) -> JointEnv<'a> {
        JointEnv {
            models: &self.models,
            state,
            owner,
            blind,
        }
    }

    /// Walk every reachable joint state breadth first, checking `invariants`
    /// of `owner` in each settled one (nothing in flight).
    pub(super) fn walk(&self, owner: &str, invariants: &[&Expr], budget: Budget) -> Walk {
        let started = Instant::now();
        let mut nodes: Vec<(JointState, Option<(usize, String)>)> = Vec::new();
        let mut seen: HashMap<JointState, usize> = HashMap::new();
        let mut queue = VecDeque::new();
        for init in self.inits() {
            if !seen.contains_key(&init) {
                seen.insert(init.clone(), nodes.len());
                queue.push_back(nodes.len());
                nodes.push((init, None));
            }
        }
        let mut broken: Vec<Option<usize>> = vec![None; invariants.len()];
        let mut stopped = None;
        let mut held_back = false;
        while let Some(index) = queue.pop_front() {
            let state = nodes[index].0.clone();
            if state.inflight.is_empty() {
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
            for mv in self.moves(&state) {
                let (nexts, held) = self.next(&state, &mv);
                held_back |= held;
                for next in nexts {
                    if seen.contains_key(&next.state) {
                        continue;
                    }
                    let at = nodes.len();
                    seen.insert(next.state.clone(), at);
                    nodes.push((next.state, Some((index, next.label))));
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
        Walk {
            states: nodes.len(),
            outcomes,
            held_back,
        }
    }
}

/// Every way to split `n` items into blocks, as each item's block number,
/// blocks numbered in order of first use.
fn partitions(n: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new()];
    for _ in 0..n {
        out = out
            .into_iter()
            .flat_map(|blocks| {
                let used = blocks.iter().max().map_or(0, |most| most + 1);
                (0..=used).map(move |block| {
                    let mut more = blocks.clone();
                    more.push(block);
                    more
                })
            })
            .collect();
    }
    out
}

/// Renumber new strings over the whole joint state, records first, then
/// the dispatches in flight; returns each value's new name.
fn canonical(state: &mut JointState) -> BTreeMap<String, String> {
    let mut dispatches = std::mem::take(&mut state.inflight);
    let mut strings: Vec<BTreeMap<String, String>> = dispatches
        .iter()
        .map(|d| {
            d.params
                .iter()
                .filter_map(|(name, value)| match value {
                    ParamValue::Str(text) => Some((name.clone(), text.clone())),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let renamed = {
        let mut maps: Vec<&mut BTreeMap<String, String>> = state
            .records
            .values_mut()
            .map(|s| &mut s.strings)
            .chain(strings.iter_mut())
            .collect();
        canonical_strings(&mut maps)
    };
    for (dispatch, renamed_strings) in dispatches.iter_mut().zip(strings) {
        for (name, text) in renamed_strings {
            dispatch.params.insert(name, ParamValue::Str(text));
        }
    }
    dispatches.sort();
    state.inflight = dispatches;
    renamed
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
                    resolve: trigger.resolve_target.clone(),
                });
            }
        }
    }
    reactions
}
