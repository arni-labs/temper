//! The joint model: the records in scope, their reactions, the dispatches
//! and module calls on their way, who may send what, and the walk.

mod commit;
mod order;
mod react;
mod read;
mod step;
mod walk;

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::{May, SendAnalysis};
use temper_spec::automaton::{Automaton, TargetResolver};
use temper_spec::predicate::{Arg, Expr, Name, Truth, eval};

use super::env::{Blind, ID, JointEnv};
use super::senders::{OUTSIDE, Senders};
use super::slice::slice;
use super::state::canonical;
use super::{JointOutcome, JointState};
use crate::Budget;
use crate::model::semantics::string_values;
use crate::model::{
    ParamValue, ResolvedTransition, TemperModel, TemperModelState, build_model_tracking,
};

use order::{Order, Specs};
pub(super) use read::Timeout;
use read::{Kernel, Reaction, Unchecked, named_literals, reactions_in, unchecked_constraints};
pub(super) use step::Move;
pub(super) use walk::Explored;

/// Counter ceiling for the joint models, as for the composite plans.
const JOINT_MAX_COUNTER: usize = 3;

/// Reaction depth followed: the runtime's own bound.
const MAX_REACTION_DEPTH: u32 = temper_runtime::reaction::MAX_REACTION_DEPTH;

/// Dispatches and module calls in flight at once. The runtime has no such
/// bound: each request, reaction and module call is its own task. The model
/// needs one to stay finite; three lets a late dispatch land after two later
/// ones. A walk that held a move back for it says so, and then proves only
/// up to this many.
pub(super) const MAX_IN_FLIGHT: usize = 3;

/// Who sends what in the model.
pub(super) struct Sending<'s> {
    /// The policies that decide each move as its sender; `None` lets every
    /// sender send everything.
    pub(super) policies: Option<&'s SendAnalysis>,
    /// Whether the model makes the kernel's own moves: state timers, module
    /// answers and failures, and module calls lost to a restart. Without
    /// them anyone may send any action, which covers what they do.
    pub(super) kernel_moves: bool,
    /// Actions (entity, action) that code outside the specs sends, offered
    /// and allowed whatever the policies say.
    pub(super) relies_on: BTreeSet<(String, String)>,
}

impl Sending<'_> {
    /// Anyone sends anything.
    pub(super) fn anyone() -> Self {
        Self {
            policies: None,
            kernel_moves: false,
            relies_on: BTreeSet::new(),
        }
    }
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

/// The joint model of one entity's properties that read related entities.
pub(super) struct JointModel {
    models: BTreeMap<String, TemperModel>,
    reactions: Vec<Reaction>,
    /// The types composed: those the properties name.
    pub(super) scope: BTreeSet<String>,
    /// Whether the kernel's own moves are made as the kernel makes them.
    kernel_moves: bool,
    kernel: Kernel,
    relies_on: BTreeSet<(String, String)>,
    /// The actions with a constraint the model does not decide.
    unchecked: Unchecked,
    /// The counts kept as sequence numbers.
    order: Order,
    /// Who sends each move, and what Cedar lets them send.
    senders: Senders,
    /// The entities whose moves' senders matter: those with a reaction that
    /// runs as whoever sent its source action, which Cedar decides as them.
    /// Elsewhere who armed a timer or answers a call changes nothing, so the
    /// model does not tell them apart.
    sender_matters: BTreeSet<String>,
}

impl JointModel {
    /// The model of `owner`'s properties `reads` over `scope`. With `cut`,
    /// each entity is cut down to what `reads` depends on, which only adds
    /// behaviour.
    pub(super) fn new(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: BTreeSet<String>,
        owner: &str,
        reads: &[&Expr],
        sending: Sending<'_>,
        cut: bool,
    ) -> Self {
        // What the properties read: the owner's variables and the id fields
        // its references use, and the related types' fields.
        let mut read: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut owned = |entity: &str, field: &str| {
            if !matches!(field, "Id" | "id") {
                read.entry(entity.to_string())
                    .or_default()
                    .insert(field.to_string());
            }
        };
        for expr in reads {
            expr.for_each_name(&mut |n| match n {
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
        let (mut models, reactions) = loop {
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
        for ((entity, field), literals) in named_literals(owner, reads, &models, &reactions) {
            if let Some(model) = models.get_mut(&entity) {
                model
                    .string_literals
                    .entry(field)
                    .or_default()
                    .extend(literals);
            }
        }
        let kernel = if sending.kernel_moves {
            Kernel::read(by_name, &scope)
        } else {
            Kernel::default()
        };
        let senders = Senders::new(by_name, &scope, sending.policies);
        let chosen = |entity: &str, action: &str| {
            sending
                .relies_on
                .contains(&(entity.to_string(), action.to_string()))
                || senders.may(OUTSIDE, entity, action) != May::No
                || kernel.chooses(entity, action)
        };
        let order = Order::read(&Specs {
            by_name,
            models: &models,
            reactions: &reactions,
            read: &read,
            chosen: &chosen,
        });
        let models: BTreeMap<String, TemperModel> = models
            .into_iter()
            .map(|(entity, model)| {
                let mut keep = read.get(&entity).cloned().unwrap_or_default();
                keep.extend(order.fields(&entity).map(|(field, _)| field.to_string()));
                let mut model = if cut { slice(model, keep) } else { model };
                order.bound(&entity, &mut model);
                (entity, model)
            })
            .collect();
        let unchecked = unchecked_constraints(by_name, &models, &order);
        let sender_matters = reactions
            .iter()
            .filter(|r| r.principal.is_none())
            .map(|r| r.from.clone())
            .collect();
        Self {
            models,
            reactions,
            scope,
            kernel_moves: sending.kernel_moves,
            kernel,
            relies_on: sending.relies_on,
            unchecked,
            order,
            senders,
            sender_matters,
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
                    calls: Vec::new(),
                    armed: BTreeMap::new(),
                };
                self.arm_at_start(&mut state);
                self.order.normalize(&mut state);
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
            calls: Vec::new(),
            armed: BTreeMap::new(),
        };
        self.arm_at_start(&mut state);
        self.order.normalize(&mut state);
        let renamed = canonical(&mut state);
        for value in names.values_mut() {
            if let Some(new) = renamed.get(value) {
                *value = new.clone();
            }
        }
        (state, names)
    }

    /// A record that starts in a state with a timer was made by someone
    /// outside, who armed it.
    fn arm_at_start(&self, state: &mut JointState) {
        for (entity, record) in &state.records {
            if self.sender_matters.contains(entity)
                && self.timeout(entity, &record.status).is_some()
            {
                state.armed.insert(entity.clone(), OUTSIDE);
            }
        }
    }

    /// `entity`'s timer in `status`, when the model makes timers fire.
    pub(super) fn timeout(&self, entity: &str, status: &str) -> Option<&Timeout> {
        self.kernel
            .timers
            .get(entity)?
            .iter()
            .find(|timeout| timeout.state == status)
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
        for call in &state.calls {
            values.extend(call.asked.values().filter(|v| !v.is_empty()).cloned());
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
        let mut broken: Vec<Option<usize>> = vec![None; invariants.len()];
        let explored = self.walk_with(budget, false, |index, state| {
            if state.settled() {
                let env = self.env(owner, state);
                for (i, inv) in invariants.iter().enumerate() {
                    if broken[i].is_none() && eval(inv, &env) == Truth::False {
                        broken[i] = Some(index);
                    }
                }
            }
            broken.iter().all(Option::is_some)
        });
        let outcomes = broken
            .iter()
            .map(|found| match found {
                Some(index) => JointOutcome::Broken {
                    path: explored.path(*index),
                },
                None => match &explored.stopped {
                    Some(reason) => JointOutcome::Incomplete {
                        reason: reason.clone(),
                    },
                    None => JointOutcome::Holds,
                },
            })
            .collect();
        Walk {
            states: explored.len(),
            outcomes,
            held_back: explored.held_back,
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
