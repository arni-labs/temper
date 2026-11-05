//! What a rule reads, and which moves can change it: whether a move the
//! walk held back for [`JointModel::in_flight`] matters to the rule.
//!
//! The cone starts with the fields and statuses the rule reads and the
//! fields Cedar reads. It grows, until nothing more comes in, by what
//! decides each move that can change it: the move's status, guard,
//! constraints, effect inputs and the counters and lists it grows (a step
//! past a bound is cut); the guard, arguments and target of each reaction
//! that sends it; and a whole family of counts kept as sequence numbers
//! when it holds one of them. The moves that can change it are those that
//! write it, those whose reactions, module answers (named, or any a module
//! naming none may give) or timer resets can lead to one, and the reactions
//! a commit sends before one of those with all they lead to: the runtime
//! sends a commit's reactions one trigger at a time, each with its cascade
//! ([`JointModel::overtakes`]). A dispatch or call in flight matters when
//! the move it leads to can. Where a rule reads a record a spawn makes, or
//! a move that spawns can change it, or reads a type of several records a
//! reaction makes one of, every move counts: which record that takes, and
//! whether a spawn goes on, turn on all of each and on what is on its way
//! to it (`spawn.rs`, `copies.rs`).
//!
//! Why growing only when a held move matters is enough. Take any run, with
//! as many in flight as it likes, and a walked state that agrees with where
//! the run is on the cone, on what in flight matters, and on the armed
//! timers that matter. A move of the run that changes none of those leaves
//! that state as good as it was. A move that changes one reads only those,
//! so the walked state can make it too, the same ways, with the same Cedar
//! answers and certainty, and agrees with where it leads. The walk keeps
//! that, unless it does not fit, and then the walk held back a move that
//! reaches the cone ([`Cone::reaches`]) and grew. So every state a run
//! reaches agrees with one the walk has, and a rule, which reads only the
//! cone, says the same of both.
//!
//! Two things could undo that, and are handled so:
//! - An outside move may send any string the state holds
//!   ([`JointModel::values`]), and a dispatch the walked state lacks may
//!   carry one. The walk also offers it a value new to the state
//!   (`successors_among`), and the cone cannot tell the two apart: neither
//!   equals a value it holds or a literal it names.
//! - A rule is read, and a liveness condition counts as reached, where
//!   nothing is in flight, and dispatches that cannot reach the cone can
//!   keep a state from emptying, in the run or in the walk. So both are read
//!   where nothing in flight can change what they read ([`Cone::quiet`]).
//!   That finds what reading only empty states finds: from a quiet state
//!   the system can land all in flight, each landing changing nothing read
//!   and setting off only more such moves; a call may always end with no
//!   answer, and the runtime stops a reaction its chain already holds, so
//!   the landing ends in an empty state that reads the same. Only a fair
//!   cycle that keeps something in flight forever where the condition holds
//!   now counts as reaching it.
//!
//! A fair cycle counts a delivery held back as one the runtime keeps able to
//! make, whatever it changes, so a rule with fairness grows on every move
//! held back (`proof.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Arg, Expr, Name};

use super::super::env::ID;
use super::super::{Call, Dispatch, JointState};
use super::copies::base;
use super::{HeldBack, JointModel};
use crate::model::{ModelEffect, ResolvedTransition, TemperModelState};

/// What a rule reads, and the moves that can change it, by type.
#[derive(Debug, Default)]
pub(in crate::composite::joint) struct Cone {
    /// Every move counts: a model made for no rule.
    whole: bool,
    /// Each type's fields read.
    fields: BTreeMap<String, BTreeSet<String>>,
    /// The types whose status is read.
    statuses: BTreeSet<String>,
    /// Each type's actions that can change what is read.
    actions: BTreeMap<String, BTreeSet<String>>,
    /// The types with a state timer whose action can.
    timers: BTreeSet<String>,
    /// The types whose calls naming no answer can answer with one that can.
    open: BTreeSet<String>,
}

impl Cone {
    /// The cone of a model made for no rule: every move counts.
    pub(super) fn whole() -> Self {
        Self {
            whole: true,
            ..Self::default()
        }
    }

    fn read_field(&mut self, entity: &str, field: &str) -> bool {
        let field = if field == "id" { ID } else { field };
        self.fields
            .entry(entity.to_string())
            .or_default()
            .insert(field.to_string())
    }

    fn read_status(&mut self, entity: &str) -> bool {
        self.statuses.insert(entity.to_string())
    }

    /// Read what `expr`, said of `entity`, names.
    fn read_expr(&mut self, entity: &str, expr: &Expr) -> bool {
        let mut grew = false;
        expr.for_each_name(&mut |name| match name {
            Name::Status => grew |= self.read_status(entity),
            Name::Var(field) => grew |= self.read_field(entity, field),
            Name::CrossStatus {
                entity_type,
                id_field,
            } => {
                grew |= self.read_field(entity, id_field);
                grew |= self.read_field(entity_type, ID);
                grew |= self.read_status(entity_type);
            }
            Name::CrossField {
                entity_type,
                id_field,
                field,
            } => {
                grew |= self.read_field(entity, id_field);
                grew |= self.read_field(entity_type, ID);
                grew |= self.read_field(entity_type, field);
            }
            Name::Param(_) => {}
        });
        grew
    }

    fn reads(&self, entity: &str, field: &str) -> bool {
        self.fields.get(entity).is_some_and(|f| f.contains(field))
    }

    /// Whether anything of `entity`'s is read.
    fn touches(&self, entity: &str) -> bool {
        self.statuses.contains(entity) || self.fields.get(entity).is_some_and(|f| !f.is_empty())
    }

    fn add_action(&mut self, entity: &str, action: &str) -> bool {
        self.actions
            .entry(entity.to_string())
            .or_default()
            .insert(action.to_string())
    }

    /// Whether `entity`'s `action` can change what is read.
    pub(in crate::composite::joint) fn counts(&self, entity: &str, action: &str) -> bool {
        self.whole
            || self
                .actions
                .get(base(entity))
                .is_some_and(|actions| actions.contains(action))
    }

    /// Whether `t`, a transition of `entity`, writes what is read.
    fn written_by(&self, entity: &str, t: &ResolvedTransition) -> bool {
        let moves = t.to_state.is_some_and(|to| {
            t.from_states.is_empty() || t.from_states.iter().any(|from| *from != to)
        });
        (moves && self.statuses.contains(entity))
            || t.effects.iter().any(|e| self.reads(entity, effect_of(e).0))
            || t.string_params
                .iter()
                .any(|p| p.writes && self.reads(entity, &p.name))
    }

    /// Whether `dispatch` can lead to a move that changes what is read.
    pub(in crate::composite::joint) fn sends(&self, dispatch: &Dispatch) -> bool {
        self.counts(dispatch.to.as_str(), dispatch.action.as_str())
    }

    /// Whether `call` can be answered with a move that changes what is read.
    pub(in crate::composite::joint) fn answers(&self, call: &Call) -> bool {
        self.whole
            || call
                .on_success
                .iter()
                .any(|a| self.counts(call.to.as_str(), a.as_str()))
            || call
                .on_failure
                .is_some_and(|a| self.counts(call.to.as_str(), a.as_str()))
            || (call.on_success.is_empty() && self.open.contains(base(call.to.as_str())))
    }

    /// How many things in flight in `state` can change what is read.
    pub(in crate::composite::joint) fn in_flight(&self, state: &JointState) -> usize {
        let dispatches = state.inflight.iter().filter(|d| self.sends(d)).count();
        dispatches + state.calls.iter().filter(|c| self.answers(c)).count()
    }

    /// Whether nothing in flight in `state` can change what is read: what
    /// lands next leaves it as it is.
    pub(in crate::composite::joint) fn quiet(&self, state: &JointState) -> bool {
        if self.whole {
            return state.settled();
        }
        !state.inflight.iter().any(|d| self.sends(d))
            && !state.calls.iter().any(|c| self.answers(c))
    }

    /// Whether `after`, where a move from `before` leads, differs from it
    /// in what is read, in what in flight can change it, or in a timer armed
    /// that can.
    pub(in crate::composite::joint) fn reaches(
        &self,
        before: &JointState,
        after: &JointState,
    ) -> bool {
        if self.whole {
            return true;
        }
        for (name, now) in after.records.iter() {
            let entity = base(name.as_str());
            let Some(was) = before.records.get(name.as_str()) else {
                return true;
            };
            if (self.statuses.contains(entity) && was.status != now.status)
                || self.timers.contains(entity)
                    && before.armed.get(name.as_str()) != after.armed.get(name.as_str())
                || self
                    .fields
                    .get(entity)
                    .is_some_and(|fields| fields.iter().any(|f| differs(was, now, f)))
            {
                return true;
            }
        }
        let sent = |s: &JointState| -> Vec<Arc<Dispatch>> {
            s.inflight
                .iter()
                .filter(|d| self.sends(d))
                .cloned()
                .collect()
        };
        let asked = |s: &JointState| -> Vec<Arc<Call>> {
            s.calls
                .iter()
                .filter(|c| self.answers(c))
                .cloned()
                .collect()
        };
        sent(before) != sent(after) || asked(before) != asked(after)
    }
}

/// Whether `field` holds another value in `now` than in `was`.
fn differs(was: &TemperModelState, now: &TemperModelState, field: &str) -> bool {
    was.strings.get(field) != now.strings.get(field)
        || was.counters.get(field) != now.counters.get(field)
        || was.booleans.get(field) != now.booleans.get(field)
        || was.lists.get(field) != now.lists.get(field)
}

/// The field an effect writes, what it reads, and whether it can go past a
/// bound, which cuts the step.
fn effect_of(effect: &ModelEffect) -> (&str, Option<&Arg>, bool) {
    match effect {
        ModelEffect::SetCounter { var, value } | ModelEffect::AddCounter { var, value } => {
            (var.as_str(), Some(value), true)
        }
        ModelEffect::ListAppend { var, value } => (var.as_str(), Some(value), true),
        ModelEffect::SubCounter { var, value }
        | ModelEffect::SetBool { var, value }
        | ModelEffect::ListRemove { var, value } => (var.as_str(), Some(value), false),
        ModelEffect::ListRemoveAt { var, index } => (var.as_str(), Some(index), false),
        ModelEffect::SetString { var, .. } => (var.as_str(), None, false),
    }
}

impl JointModel {
    /// What holding back `after`, where a move from `before` leads, holds back.
    pub(in crate::composite::joint) fn held_back(
        &self,
        before: &JointState,
        after: &JointState,
    ) -> HeldBack {
        if self.cone.reaches(before, after) {
            HeldBack::Reaching
        } else {
            HeldBack::Aside
        }
    }

    /// Whether nothing in flight in `state` can change what the rule reads.
    pub(in crate::composite::joint) fn quiet(&self, state: &JointState) -> bool {
        self.cone.quiet(state)
    }

    /// With several records of a type, a reaction that makes one takes the
    /// record that all of each one and what is on its way to it choose
    /// (`copies.rs`). Where a rule reads such a type, every move counts.
    pub(super) fn cone_for_copies(&mut self) {
        let makes = |r: &super::read::Reaction| {
            matches!(
                r.resolve,
                Some(TargetResolver::Create | TargetResolver::CreateIfMissing { .. })
            ) && self.cone.touches(&r.to)
        };
        self.cone.whole |= self.reactions.iter().any(makes);
    }

    /// The cone of the rules `reads`, said of `owner`.
    pub(super) fn cone_of(&self, owner: &str, reads: &[&Expr]) -> Cone {
        let mut cone = Cone::default();
        for expr in reads {
            cone.read_expr(owner, expr);
        }
        for (entity, model) in &self.models {
            for field in model.var_kinds.keys() {
                if self.senders.reads_field(field) {
                    cone.read_field(entity, field);
                }
            }
            if self.senders.reads_field("status") {
                cone.read_status(entity);
            }
        }
        while self.grow(&mut cone) {}
        cone
    }

    /// One round of the cone's growth; whether it grew.
    fn grow(&self, cone: &mut Cone) -> bool {
        if cone.whole {
            return false;
        }
        let mut grew = false;
        for (entity, model) in &self.models {
            for t in &model.transitions {
                if cone.written_by(entity, t) {
                    grew |= cone.add_action(entity, &t.name);
                }
            }
        }
        for ((entity, action), fields) in &self.outside_spawns {
            if fields.iter().any(|f| cone.reads(entity, f)) {
                grew |= cone.add_action(entity, action);
            }
        }
        grew |= self.grow_by_reactions(cone);
        for ((entity, action), specs) in &self.kernel.calls_on {
            // A module whose trigger names no answer may answer with any
            // internal action, or any that nothing else can send (`moves.rs`).
            let internal = self.kernel.internal.get(entity).into_iter().flatten();
            let unsent = self.unsent.get(entity).into_iter().flatten();
            let open = internal.map(String::as_str).any(|a| cone.counts(entity, a))
                || unsent
                    .map(|&way| self.models[entity].transitions[way].name.as_str())
                    .any(|a| cone.counts(entity, a));
            if open {
                grew |= cone.open.insert(entity.clone());
            }
            let answers = specs.iter().any(|spec| {
                spec.on_success.iter().any(|a| cone.counts(entity, a))
                    || spec
                        .on_failure
                        .as_ref()
                        .is_some_and(|a| cone.counts(entity, a))
                    || (spec.on_success.is_empty() && open)
            });
            if answers {
                grew |= cone.add_action(entity, action);
            }
        }
        for (entity, timers) in &self.kernel.timers {
            let counted: Vec<_> = timers
                .iter()
                .filter(|t| cone.counts(entity, &t.action))
                .collect();
            for timer in counted {
                grew |= cone.timers.insert(entity.clone());
                for action in &timer.reset_on {
                    grew |= cone.add_action(entity, action);
                }
            }
        }
        for (entity, actions) in cone.actions.clone() {
            grew |= cone.read_status(&entity);
            let Some(model) = self.models.get(&entity) else {
                continue;
            };
            for t in model
                .transitions
                .iter()
                .filter(|t| actions.contains(t.name.as_str()))
            {
                grew |= cone.read_expr(&entity, &t.guard);
                for c in &t.constraints {
                    grew |= c.field().is_some_and(|f| cone.read_field(&entity, f));
                }
                for (var, arg, bounded) in t.effects.iter().map(effect_of) {
                    if bounded {
                        grew |= cone.read_field(&entity, var);
                    }
                    if let Some(Arg::Var(field)) = arg {
                        grew |= cone.read_field(&entity, field);
                    }
                }
            }
        }
        for family in self.order.family_fields() {
            if family.iter().any(|(e, f)| cone.reads(e, f)) {
                for (entity, field) in family {
                    grew |= cone.read_field(entity, field);
                }
            }
        }
        grew
    }

    /// The reactions' part of a round of the cone's growth.
    fn grow_by_reactions(&self, cone: &mut Cone) -> bool {
        let mut grew = false;
        for (at, r) in self.reactions.iter().enumerate() {
            // Whether a spawn may put a new record in place of the one held,
            // and whether its move goes on at all, turns on all of that
            // record, on what is on its way to it, and on whether any record
            // still names it (`spawn.rs`). Where a rule reads the record a
            // spawn makes, or the move that spawns, every move counts.
            if r.spawn.is_some() && (cone.touches(&r.to) || cone.counts(&r.from, &r.source_action))
            {
                cone.whole = true;
                return true;
            }
            let made = matches!(
                r.resolve,
                Some(TargetResolver::Create | TargetResolver::CreateIfMissing { .. })
            );
            if made && cone.touches(&r.to) {
                grew |= cone.add_action(&r.to, &r.target_action);
            }
            // The source keeps the new record's id.
            let stores = match (&r.spawn, &r.resolve) {
                (Some(spawn), _) => spawn.store_id_in.as_deref(),
                (_, Some(TargetResolver::CreateIfMissing { id_field })) => Some(id_field.as_str()),
                _ => None,
            };
            if stores.is_some_and(|field| cone.reads(&r.from, field)) {
                grew |= cone.add_action(&r.from, &r.source_action);
            }
            if !cone.counts(&r.to, &r.target_action) {
                continue;
            }
            grew |= cone.add_action(&r.from, &r.source_action);
            if let Some(guard) = &r.guard {
                grew |= cone.read_expr(&r.from, guard);
            }
            for arg in r.args.values() {
                if let Arg::Var(field) = arg {
                    grew |= cone.read_field(&r.from, field);
                }
            }
            match &r.resolve {
                Some(
                    TargetResolver::Field { id_field }
                    | TargetResolver::CreateIfMissing { id_field },
                ) => grew |= cone.read_field(&r.from, id_field),
                Some(TargetResolver::SameId) => grew |= cone.read_field(&r.from, ID),
                _ => {}
            }
            grew |= cone.read_field(&r.to, ID);
            // The reactions this commit sends first, with their cascades,
            // land before this one does.
            if r.spawn.is_none() {
                for earlier in self.reactions[..at].iter().filter(|e| {
                    e.spawn.is_none() && e.from == r.from && e.source_action == r.source_action
                }) {
                    grew |= self.cascade_counts(cone, &earlier.to, &earlier.target_action);
                }
            }
        }
        grew
    }

    /// Count `entity`'s `action` and every reaction it sets off, on and on.
    fn cascade_counts(&self, cone: &mut Cone, entity: &str, action: &str) -> bool {
        let mut grew = false;
        let mut next = vec![(entity.to_string(), action.to_string())];
        let mut seen = BTreeSet::new();
        while let Some((entity, action)) = next.pop() {
            if !seen.insert((entity.clone(), action.clone())) {
                continue;
            }
            grew |= cone.add_action(&entity, &action);
            next.extend(
                self.reactions
                    .iter()
                    .filter(|r| r.from == entity && r.source_action == action)
                    .map(|r| (r.to.clone(), r.target_action.clone())),
            );
        }
        grew
    }
}
