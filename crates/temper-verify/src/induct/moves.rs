//! The moves of the joint state, each a formula over `pre` and `post`:
//!
//! - an action someone outside may send, with any parameters;
//! - a state timer's action, with its arguments, while its state holds;
//! - a reaction from a type outside the scope, carrying values the proof
//!   does not know, as the sender Cedar decides it for;
//! - a spawn's first action on a record of the scope, by any record, with
//!   any parameters: the child may be of another id than the scope's;
//! - a dispatch in flight arriving, or a scheduled action firing: its
//!   record takes it, sending what that sets off, or refuses it;
//! - a call in flight answered, with the trigger's declared answer or else
//!   any action of its record, as the kernel takes it, with independently
//!   chosen parameters; or failing, which fires the trigger's `on_failure` with no
//!   parameters; or ending unanswered (refused, or lost in a restart).
//!
//! A timer may fire whenever its state holds and a dispatch or call may
//! arrive at any time, so the moves allow everything the joint walk does,
//! and more.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use temper_authz::May;
use temper_spec::automaton::{Action, ActionConstraint, ActionKind, StateTimeout, TriggerKind};
use temper_spec::predicate::Effect;

use super::encode::Enc;
use super::vocab::{Call, Kind, Sort};

/// One move: its name, the constants it makes up, what lets it happen, and
/// the state it leads to.
pub(super) struct Move {
    pub(super) label: String,
    pub(super) decls: Vec<String>,
    /// Read on `pre`, and on `w` when it has one.
    pub(super) guard: Vec<String>,
    /// `post` from `pre`.
    pub(super) defs: Vec<String>,
    /// The kind of dispatch or call `w` is, when the move takes one.
    pub(super) witness: Option<String>,
    pub(super) step: Step,
    /// The record and the action it takes, when it takes one.
    pub(super) entity: String,
    pub(super) action: String,
    /// A timer's state.
    pub(super) timer: Option<String>,
}

/// What a move is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Outside,
    Timer,
    Outer,
    Arrive,
    Refuse,
    Answer,
    Fail,
    FailRefused,
    Unanswered,
}

impl Step {
    /// Whether the system makes it (a timer, a module, the kernel's
    /// dispatches) rather than someone outside or a restart.
    pub(super) fn by_system(self) -> bool {
        !matches!(self, Step::Outside | Step::Outer | Step::Unanswered)
    }
}

impl Move {
    pub(super) fn body(&self) -> impl Iterator<Item = &String> {
        self.guard.iter().chain(&self.defs)
    }
}

/// Where an action comes from.
enum Source<'t> {
    Outside,
    Timer(&'t StateTimeout),
    /// A reaction from outside the scope, carrying these parameters.
    Outer(BTreeSet<String>),
    /// A spawn's first action, passed any of the parent's parameters.
    Spawn,
    /// A dispatch of this kind arriving, as the constant `w`.
    Arrive(&'t Kind),
    /// A call of this kind answered, as the constant `w`.
    Answer(&'t Call),
    /// A call of this kind failing, as the constant `w`: its `on_failure`
    /// fires with no parameters.
    Fail(&'t Call),
}

/// A parameter's term, and whether it was passed.
pub(super) struct Param {
    pub(super) term: String,
    pub(super) passed: String,
}

/// Builds the moves of one rule's proof.
pub(super) struct Moves<'e, 'v, 'a> {
    pub(super) enc: &'e Enc<'v, 'a>,
    /// What the encoding could not express, in words.
    pub(super) limits: RefCell<BTreeSet<String>>,
    /// Parameters a liveness proof chooses for a helpful move, by name, as
    /// terms over `pre` and `w`; `passed.<name>` says whether it is passed.
    pins: BTreeMap<String, String>,
}

impl<'e, 'v, 'a> Moves<'e, 'v, 'a> {
    pub(super) fn new(enc: &'e Enc<'v, 'a>) -> Self {
        Self::pinned(enc, BTreeMap::new())
    }

    pub(super) fn pinned(enc: &'e Enc<'v, 'a>, pins: BTreeMap<String, String>) -> Self {
        Self {
            enc,
            limits: RefCell::default(),
            pins,
        }
    }

    /// Every move of the scope's records.
    pub(super) fn all(&self) -> Vec<Move> {
        let v = self.enc.v;
        let mut out = Vec::new();
        for entity in &v.scope {
            let automaton = v.automaton(entity);
            for action in &automaton.actions {
                if action.kind == ActionKind::Input
                    && v.outside_may(entity, &action.name) != May::No
                {
                    out.push(self.take(entity, action, &Source::Outside, "from outside"));
                }
                for timeout in automaton
                    .state_timeouts
                    .iter()
                    .filter(|t| t.on_timeout == action.name)
                {
                    let label = format!("timer in {}", timeout.state);
                    out.push(self.take(entity, action, &Source::Timer(timeout), &label));
                }
            }
        }
        for (source, automaton) in v.by_name.iter().filter(|(n, _)| !v.scope.contains(**n)) {
            for action in &automaton.actions {
                for trigger in action
                    .triggers
                    .iter()
                    .filter(|t| t.kind == TriggerKind::Entity)
                {
                    let (Some(to), Some(target)) = (&trigger.target_entity, &trigger.target_action)
                    else {
                        continue;
                    };
                    let Some(target_action) =
                        v.scope.contains(to).then(|| v.action(to, target)).flatten()
                    else {
                        continue;
                    };
                    if v.reaction_may(source, &action.name, trigger, to, target) == May::No {
                        continue;
                    }
                    let carried = trigger.args.keys().cloned().collect();
                    let label = format!("a reaction from {source}.{}", action.name);
                    out.push(self.take(to, target_action, &Source::Outer(carried), &label));
                }
            }
        }
        if v.spawn_may() != May::No {
            for (source, automaton) in &v.by_name {
                for action in &automaton.actions {
                    for effect in &action.effect {
                        let Effect::Spawn {
                            entity_type,
                            initial_action,
                            ..
                        } = effect
                        else {
                            continue;
                        };
                        let Some(first) = v
                            .scope
                            .contains(entity_type)
                            .then(|| v.action(entity_type, initial_action))
                            .flatten()
                        else {
                            continue;
                        };
                        let label = format!("a spawn from {source}.{}", action.name);
                        out.push(self.take(entity_type, first, &Source::Spawn, &label));
                    }
                }
            }
        }
        for kind in &v.kinds {
            if let Some(action) = v.action(&kind.to, &kind.action) {
                let how = if kind.scheduled { "fires" } else { "arrives" };
                if kind.may != May::No {
                    out.push(self.take(&kind.to, action, &Source::Arrive(kind), how));
                }
                out.push(self.refuse(kind, action));
            }
        }
        for call in &v.calls {
            for answer in v.answer_actions(call) {
                let how = format!("answer to {}", call.name);
                out.push(self.take(&call.to, answer, &Source::Answer(call), &how));
            }
            match call
                .on_failure
                .as_deref()
                .and_then(|name| v.action(&call.to, name))
            {
                Some(action) => {
                    out.push(self.take(&call.to, action, &Source::Fail(call), "fails"));
                    out.push(self.end(call, Some(action)));
                }
                None => out.push(self.end(call, None)),
            }
            out.push(self.end_unanswered(call));
        }
        out
    }

    /// `entity` takes `action` from `source`.
    fn take(&self, entity: &str, action: &Action, source: &Source<'_>, how: &str) -> Move {
        let label = match source {
            Source::Arrive(kind) => format!("{} {how}", kind.name),
            Source::Fail(call) => format!("{} {how}", call.name),
            _ => format!("{entity}.{} ({how})", action.name),
        };
        let mut guard = Vec::new();
        let params = self.params(entity, action, source, &mut guard);
        guard.extend(self.enabled(entity, action, &params));
        if let Source::Timer(timeout) = source {
            guard.push(format!(
                "(= {} {})",
                Enc::status("pre", entity),
                Enc::state(entity, &timeout.state)
            ));
        }
        let witness = match source {
            Source::Arrive(kind) => Some(kind.name.as_str()),
            Source::Answer(call) | Source::Fail(call) => Some(call.name.as_str()),
            _ => None,
        };
        let mut changed = BTreeMap::new();
        if let Some(name) = witness {
            guard.push(format!("(> (select pre.pend.{name} w) 0)"));
            changed = decrement(name);
        }
        self.commit(entity, action, &params, &mut changed);
        self.sends(entity, action, &params, &mut changed);
        let defs = self.enc.frame_rest("pre", "post", &changed);
        let step = match source {
            Source::Outside => Step::Outside,
            Source::Timer(_) => Step::Timer,
            Source::Outer(_) | Source::Spawn => Step::Outer,
            Source::Arrive(_) => Step::Arrive,
            Source::Answer(_) => Step::Answer,
            Source::Fail(_) => Step::Fail,
        };
        let mut made = self.made(label, guard, defs, witness, step, (entity, &action.name));
        if let Source::Timer(timeout) = source {
            made.timer = Some(timeout.state.clone());
        }
        made
    }

    /// A dispatch of `kind` arrives and its record refuses it: it cannot
    /// take it, or Cedar may refuse it.
    fn refuse(&self, kind: &Kind, action: &Action) -> Move {
        let mut guard = vec![format!("(> (select pre.pend.{0} w) 0)", kind.name)];
        let params = self.params(&kind.to, action, &Source::Arrive(kind), &mut guard);
        if kind.may == May::Yes {
            let enabled = self.enabled(&kind.to, action, &params);
            guard.push(format!("(not (and true {}))", enabled.join(" ")));
        }
        let defs = self.enc.frame_rest("pre", "post", &decrement(&kind.name));
        let label = format!("{} refused", kind.name);
        let target = (kind.to.as_str(), kind.action.as_str());
        self.made(label, guard, defs, Some(&kind.name), Step::Refuse, target)
    }

    /// A call of `call`'s kind fails and its record refuses its `on_failure`
    /// (`fails, refused`), or it fails with none (`fails`).
    fn end(&self, call: &Call, on_failure: Option<&Action>) -> Move {
        let mut guard = vec![format!("(> (select pre.pend.{0} w) 0)", call.name)];
        let (label, step, action) = match on_failure {
            Some(action) => {
                let params = self.params(&call.to, action, &Source::Fail(call), &mut guard);
                let enabled = self.enabled(&call.to, action, &params);
                guard.push(format!("(not (and true {}))", enabled.join(" ")));
                let label = format!("{} fails, refused", call.name);
                (label, Step::FailRefused, action.name.as_str())
            }
            None => (format!("{} fails", call.name), Step::Fail, ""),
        };
        let defs = self.enc.frame_rest("pre", "post", &decrement(&call.name));
        self.made(
            label,
            guard,
            defs,
            Some(&call.name),
            step,
            (&call.to, action),
        )
    }

    /// A call of `call`'s kind ends with nothing taken: its record refuses
    /// the answer, or a restart loses it.
    fn end_unanswered(&self, call: &Call) -> Move {
        let guard = vec![format!("(> (select pre.pend.{0} w) 0)", call.name)];
        let defs = self.enc.frame_rest("pre", "post", &decrement(&call.name));
        let label = format!("{} ends unanswered", call.name);
        let target = (call.to.as_str(), "");
        self.made(
            label,
            guard,
            defs,
            Some(&call.name),
            Step::Unanswered,
            target,
        )
    }

    pub(super) fn made(
        &self,
        label: String,
        guard: Vec<String>,
        defs: Vec<String>,
        witness: Option<&str>,
        step: Step,
        (entity, action): (&str, &str),
    ) -> Move {
        let mut decls = self.enc.take_decls();
        if let Some(kind) = witness {
            decls.push(format!("(declare-const w Msg/{kind})"));
        }
        Move {
            label,
            decls,
            guard,
            defs,
            witness: witness.map(str::to_string),
            step,
            entity: entity.to_string(),
            action: action.to_string(),
            timer: None,
        }
    }

    /// The terms of `action`'s parameters as `source` passes them.
    fn params(
        &self,
        entity: &str,
        action: &Action,
        source: &Source<'_>,
        guard: &mut Vec<String>,
    ) -> BTreeMap<String, Param> {
        let v = self.enc.v;
        let mut params = BTreeMap::new();
        for name in action.params.iter().map(param_name) {
            let sort = v.param(action, name).unwrap_or(Sort::Str);
            let fresh = || self.enc.fresh(&format!("p.{name}"), sort.smt());
            let (term, passed) = match source {
                Source::Arrive(kind) if kind.fields.iter().any(|(f, _)| f == name) => {
                    (format!("({}/{name} w)", kind.name), "true".to_string())
                }
                Source::Timer(timeout) if timeout.args.contains_key(name) => (
                    self.arg(entity, &timeout.args[name], sort, "pre", &BTreeMap::new()),
                    "true".to_string(),
                ),
                Source::Outer(carried) if carried.contains(name) => (fresh(), "true".to_string()),
                Source::Outside | Source::Answer(_) if self.pins.contains_key(name) => (
                    self.pins[name].clone(),
                    self.pins
                        .get(&format!("passed.{name}"))
                        .cloned()
                        .unwrap_or_else(|| "true".to_string()),
                ),
                Source::Outside | Source::Answer(_) | Source::Spawn => {
                    (fresh(), self.enc.fresh(&format!("passed.{name}"), "Bool"))
                }
                _ => (fresh(), "false".to_string()),
            };
            // A count its sender chooses is never negative; one a dispatch
            // carries was read from its sender's counter.
            let carried = matches!(source, Source::Arrive(kind) if kind.fields.iter().any(|(f, _)| f == name));
            if let (Sort::Int { counter: true }, false) = (sort, carried) {
                guard.push(format!("(>= {term} 0)"));
            }
            params.insert(name.to_string(), Param { term, passed });
        }
        params
    }

    /// What lets `entity`'s record take `action` with `params`: its state,
    /// its guard, and its constraints.
    pub(super) fn enabled(
        &self,
        entity: &str,
        action: &Action,
        params: &BTreeMap<String, Param>,
    ) -> Vec<String> {
        let status = Enc::status("pre", entity);
        let mut out = Vec::new();
        if !action.from.is_empty() {
            let from: Vec<String> = action
                .from
                .iter()
                .map(|s| format!("(= {status} {})", Enc::state(entity, s)))
                .collect();
            out.push(format!("(or {})", from.join(" ")));
        }
        out.push(self.enc.expr(&action.guard, entity, "pre"));
        for constraint in &action.constraints {
            let (param, field, rel) = match constraint {
                ActionConstraint::ParamNonempty { param } => (param, None, ""),
                ActionConstraint::ParamEqualsField { param, field } => (param, Some(field), "="),
                ActionConstraint::ParamNotEqualsField { param, field } => {
                    (param, Some(field), "distinct")
                }
                ActionConstraint::ParamGreaterThanField { param, field } => {
                    (param, Some(field), ">")
                }
            };
            let Some(p) = params.get(param) else {
                // A constraint on a parameter nobody can pass refuses it.
                out.push("false".to_string());
                continue;
            };
            out.push(p.passed.clone());
            match field {
                None if self.enc.v.param(action, param) == Some(Sort::Str) => {
                    out.push(format!("(not (= {} {}))", p.term, Enc::lit("")));
                }
                None => {}
                Some(field) => {
                    let Some(sort) = self.enc.v.var(entity, field) else {
                        self.limits.borrow_mut().insert(format!(
                            "{entity}.{}: a constraint reads {field}, which the proof does not hold",
                            action.name
                        ));
                        continue;
                    };
                    // A parameter of another sort, or with no type, is
                    // converted by the kernel: any value of the field's.
                    let same = self.enc.v.param(action, param).map(Sort::smt) == Some(sort.smt());
                    let term = if same {
                        p.term.clone()
                    } else {
                        self.enc.fresh(&format!("p.{param}"), sort.smt())
                    };
                    out.push(format!("({rel} {term} {})", Enc::var("pre", entity, field)));
                }
            }
        }
        out
    }
}

fn param_name(param: &temper_spec::automaton::ActionParam) -> &str {
    match param {
        temper_spec::automaton::ActionParam::Named(name) => name,
        temper_spec::automaton::ActionParam::Typed { name, .. } => name,
    }
}

/// One of kind `name`, the constant `w`, leaves the flight.
fn decrement(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            format!("post.pend.{name}"),
            format!("(store pre.pend.{name} w (- (select pre.pend.{name} w) 1))"),
        ),
        (format!("post.n.{name}"), format!("(- pre.n.{name} 1)")),
    ])
}
