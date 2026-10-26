//! The moves of the joint state, each a formula over `pre` and `post`:
//!
//! - an action someone outside may send, with any parameters;
//! - a module call's answer: an internal action, or one a trigger names as
//!   its answer or failure, with any parameters, whenever the record can
//!   take it (the proof does not hold the calls in flight, so it may come
//!   at any time);
//! - a state timer's action, with its arguments, while its state holds;
//! - a reaction from a type outside the scope, carrying values the proof
//!   does not know, as the sender Cedar decides it for;
//! - a dispatch in flight arriving: its record takes it, sending what that
//!   sets off, or refuses it.
//!
//! None of them asks for a module call or timer to be open, and a dispatch
//! may arrive at any time, so the moves allow everything the kernel does,
//! and more.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use temper_authz::May;
use temper_spec::automaton::{Action, ActionConstraint, ActionKind, StateTimeout, TriggerKind};
use temper_spec::predicate::{Arg, AssignOp, Effect};

use super::encode::Enc;
use super::vocab::{Kind, Sort};

/// One move: its name, the constants it makes up, and what holds of it.
pub(super) struct Move {
    pub(super) label: String,
    pub(super) decls: Vec<String>,
    pub(super) body: Vec<String>,
}

/// Where an action comes from.
enum Source<'t> {
    Outside,
    Answer,
    Timer(&'t StateTimeout),
    /// A reaction from outside the scope, carrying these parameters.
    Outer(BTreeSet<String>),
    /// A dispatch of this kind arriving, as the constant `w`.
    Arrive(&'t Kind),
}

/// A parameter's term, and whether it was passed.
struct Param {
    term: String,
    passed: String,
}

/// Builds the moves of one rule's proof.
pub(super) struct Moves<'e, 'v, 'a> {
    enc: &'e Enc<'v, 'a>,
    /// What the encoding could not express, in words.
    pub(super) limits: RefCell<BTreeSet<String>>,
}

impl<'e, 'v, 'a> Moves<'e, 'v, 'a> {
    pub(super) fn new(enc: &'e Enc<'v, 'a>) -> Self {
        Self {
            enc,
            limits: RefCell::default(),
        }
    }

    /// Every move of the scope's records.
    pub(super) fn all(&self) -> Vec<Move> {
        let v = self.enc.v;
        let mut out = Vec::new();
        for entity in &v.scope {
            let automaton = v.automaton(entity);
            let answers = v.answers(entity);
            for action in &automaton.actions {
                if action.kind == ActionKind::Input
                    && v.outside_may(entity, &action.name) != May::No
                {
                    out.push(self.take(entity, action, &Source::Outside, "from outside"));
                }
                if answers.contains(&action.name) {
                    out.push(self.take(entity, action, &Source::Answer, "answer"));
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
        for kind in &v.kinds {
            if let Some(action) = v.action(&kind.to, &kind.action) {
                if kind.may != May::No {
                    out.push(self.take(&kind.to, action, &Source::Arrive(kind), "arrives"));
                }
                out.push(self.refuse(kind, action));
            }
        }
        out
    }

    /// `entity` takes `action` from `source`.
    fn take(&self, entity: &str, action: &Action, source: &Source<'_>, how: &str) -> Move {
        let label = match source {
            Source::Arrive(kind) => format!("{} {how}", kind.name),
            _ => format!("{entity}.{} ({how})", action.name),
        };
        let mut body = Vec::new();
        let mut changed = BTreeMap::new();
        let params = self.params(entity, action, source, &mut body);
        body.extend(self.enabled(entity, action, &params));
        if let Source::Timer(timeout) = source {
            body.push(format!(
                "(= {} {})",
                Enc::status("pre", entity),
                Enc::state(entity, &timeout.state)
            ));
        }
        if let Source::Arrive(kind) = source {
            body.push(format!("(> (select pre.pend.{0} w) 0)", kind.name));
            changed.insert(format!("post.pend.{}", kind.name), decrement(kind));
        }
        self.commit(entity, action, &params, &mut changed);
        self.sends(entity, action, &params, &mut changed);
        body.extend(self.enc.frame_rest("pre", "post", &changed));
        let mut decls = self.enc.take_decls();
        if let Source::Arrive(kind) = source {
            decls.push(format!("(declare-const w Msg/{})", kind.name));
        }
        Move { label, decls, body }
    }

    /// A dispatch of `kind` arrives and its record refuses it: it cannot
    /// take it, or Cedar may refuse it.
    fn refuse(&self, kind: &Kind, action: &Action) -> Move {
        let mut body = vec![format!("(> (select pre.pend.{0} w) 0)", kind.name)];
        let params = self.params(&kind.to, action, &Source::Arrive(kind), &mut body);
        if kind.may == May::Yes {
            let enabled = self.enabled(&kind.to, action, &params);
            body.push(format!("(not (and true {}))", enabled.join(" ")));
        }
        let changed = BTreeMap::from([(format!("post.pend.{}", kind.name), decrement(kind))]);
        body.extend(self.enc.frame_rest("pre", "post", &changed));
        let mut decls = self.enc.take_decls();
        decls.push(format!("(declare-const w Msg/{})", kind.name));
        Move {
            label: format!("{} refused", kind.name),
            decls,
            body,
        }
    }

    /// The terms of `action`'s parameters as `source` passes them.
    fn params(
        &self,
        entity: &str,
        action: &Action,
        source: &Source<'_>,
        body: &mut Vec<String>,
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
                Source::Outside | Source::Answer => {
                    (fresh(), self.enc.fresh(&format!("passed.{name}"), "Bool"))
                }
                _ => (fresh(), "false".to_string()),
            };
            if let Sort::Int { counter: true } = sort {
                body.push(format!("(>= {term} 0)"));
            }
            params.insert(name.to_string(), Param { term, passed });
        }
        params
    }

    /// What lets `entity`'s record take `action` with `params`: its state,
    /// its guard, and its constraints.
    fn enabled(
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
                Some(field) if self.enc.v.var(entity, field).is_some() => {
                    out.push(format!(
                        "({rel} {} {})",
                        p.term,
                        Enc::var("pre", entity, field)
                    ));
                }
                Some(field) => {
                    self.limits.borrow_mut().insert(format!(
                        "{entity}.{}: a constraint reads {field}, which the proof does not hold",
                        action.name
                    ));
                }
            }
        }
        out
    }

    /// `entity`'s record after `action`: the string parameters it is passed
    /// write the variables of their names, then its effects apply in order.
    fn commit(
        &self,
        entity: &str,
        action: &Action,
        params: &BTreeMap<String, Param>,
        changed: &mut BTreeMap<String, String>,
    ) {
        let v = self.enc.v;
        let mut terms: BTreeMap<String, String> = v.vars[entity]
            .keys()
            .map(|var| (var.clone(), Enc::var("pre", entity, var)))
            .collect();
        for (name, param) in params {
            if v.var(entity, name) == Some(Sort::Str) && v.param(action, name) == Some(Sort::Str) {
                let before = &terms[name];
                let after = match param.passed.as_str() {
                    "true" => param.term.clone(),
                    "false" => before.clone(),
                    passed => format!("(ite {passed} {} {before})", param.term),
                };
                terms.insert(name.clone(), after);
            }
        }
        for effect in &action.effect {
            match effect {
                Effect::Assign { var, op, value } => {
                    let Some(sort) = v.var(entity, var) else {
                        continue;
                    };
                    let value = match value {
                        Arg::Param(p) => params
                            .get(p)
                            .map(|p| p.term.clone())
                            .unwrap_or_else(|| self.enc.fresh(&format!("p.{p}"), sort.smt())),
                        other => self.arg(entity, other, sort, "pre", &terms),
                    };
                    let now = terms[var].clone();
                    let next = match op {
                        AssignOp::Set => value,
                        AssignOp::Add => format!("(+ {now} {value})"),
                        AssignOp::Sub => format!("(ite (>= {now} {value}) (- {now} {value}) 0)"),
                    };
                    terms.insert(var.clone(), next);
                }
                Effect::Append { list, .. } | Effect::RemoveAt { list, .. } => {
                    if v.var(entity, list).is_some() {
                        self.limits
                            .borrow_mut()
                            .insert(format!("{entity}.{}: list {list}", action.name));
                    }
                }
                other => {
                    self.limits
                        .borrow_mut()
                        .insert(format!("{entity}.{}: effect {other}", action.name));
                }
            }
        }
        for (var, term) in terms {
            if term != Enc::var("pre", entity, &var) {
                changed.insert(Enc::var("post", entity, &var), term);
            }
        }
        if let Some(to) = &action.to {
            changed.insert(Enc::status("post", entity), Enc::state(entity, to));
        }
    }

    /// The dispatches `action`'s reactions put in flight, read from the
    /// record as it committed.
    fn sends(
        &self,
        entity: &str,
        action: &Action,
        params: &BTreeMap<String, Param>,
        changed: &mut BTreeMap<String, String>,
    ) {
        let v = self.enc.v;
        for trigger in &action.triggers {
            let Some(kind) = v.kind_of(entity, action, trigger) else {
                continue;
            };
            let args: Vec<String> = kind
                .fields
                .iter()
                .map(|(field, sort)| match &trigger.args[field] {
                    Arg::Param(p) => params
                        .get(p)
                        .map(|p| p.term.clone())
                        .unwrap_or_else(|| self.enc.fresh(&format!("p.{p}"), sort.smt())),
                    other => self.arg(entity, other, *sort, "post", &BTreeMap::new()),
                })
                .collect();
            let msg = if args.is_empty() {
                format!("mk/{}", kind.name)
            } else {
                format!("(mk/{} {})", kind.name, args.join(" "))
            };
            let key = format!("post.pend.{}", kind.name);
            let pend = changed
                .get(&key)
                .cloned()
                .unwrap_or_else(|| format!("pre.pend.{}", kind.name));
            let sent = format!("(store {pend} {msg} (+ (select {pend} {msg}) 1))");
            let next = match &trigger.guard {
                Some(guard) => format!(
                    "(ite {} {sent} {pend})",
                    self.enc.expr(guard, entity, "post")
                ),
                None => sent,
            };
            changed.insert(key, next);
        }
    }

    /// The term of an argument read from `entity`'s record in `frame`, or
    /// from `terms` when it holds the variable.
    fn arg(
        &self,
        entity: &str,
        arg: &Arg,
        sort: Sort,
        frame: &str,
        terms: &BTreeMap<String, String>,
    ) -> String {
        let held = match arg {
            Arg::Var(var)
                if self
                    .enc
                    .v
                    .var(entity, var)
                    .is_some_and(|s| s.smt() == sort.smt()) =>
            {
                Some(
                    terms
                        .get(var)
                        .cloned()
                        .unwrap_or_else(|| Enc::var(frame, entity, var)),
                )
            }
            Arg::Lit(literal) => Enc::literal(literal, sort),
            _ => None,
        };
        held.unwrap_or_else(|| self.enc.fresh("h", sort.smt()))
    }
}

fn param_name(param: &temper_spec::automaton::ActionParam) -> &str {
    match param {
        temper_spec::automaton::ActionParam::Named(name) => name,
        temper_spec::automaton::ActionParam::Typed { name, .. } => name,
    }
}

fn decrement(kind: &Kind) -> String {
    format!(
        "(store pre.pend.{0} w (- (select pre.pend.{0} w) 1))",
        kind.name
    )
}
