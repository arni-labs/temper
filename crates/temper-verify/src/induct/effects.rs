//! What a move writes: the record after its action, read in order from the
//! parameters it is passed and its effects, and the dispatches and module
//! calls its triggers put in flight.

use std::collections::BTreeMap;

use temper_spec::automaton::Action;
use temper_spec::predicate::{Arg, AssignOp, Effect};

use super::encode::Enc;
use super::moves::{Moves, Param};
use super::vocab::{Sort, scheduled};

impl Moves<'_, '_, '_> {
    /// `entity`'s record after `action`: the string parameters it is passed
    /// write the variables of their names, then its effects apply in order.
    pub(super) fn commit(
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
                    let (value, passed) = match value {
                        Arg::Param(p) => (
                            self.passed(action, params, p, sort),
                            params
                                .get(p)
                                .map_or("false".to_string(), |p| p.passed.clone()),
                        ),
                        other => (
                            self.arg(entity, other, sort, "pre", &terms),
                            "true".to_string(),
                        ),
                    };
                    // The kernel writes a number only when the value is a
                    // count, a non-negative integer, and a bool only when it
                    // is one; otherwise a set is skipped and an add or
                    // subtract adds nothing (`effect_args::count`).
                    let ok = match sort {
                        Sort::Int { .. } => format!("(and {passed} (>= {value} 0))"),
                        _ => passed,
                    };
                    let now = terms[var].clone();
                    let by = || format!("(ite {ok} {value} 0)");
                    let next = match op {
                        AssignOp::Set => format!("(ite {ok} {value} {now})"),
                        AssignOp::Add => format!("(+ {now} {})", by()),
                        AssignOp::Sub => {
                            // Bound once, so a chain of subtractions grows
                            // the term by a step each, not twice over.
                            let (was, less) = (self.enc.local(), self.enc.local());
                            format!(
                                "(let (({was} {now}) ({less} {})) (ite (>= {was} {less}) (- {was} {less}) 0))",
                                by()
                            )
                        }
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
                // The child's id, fresh or passed: any string. Its first
                // action is a move of its own (`Moves::all`).
                Effect::Spawn {
                    store_id_in: Some(field),
                    ..
                } if v.var(entity, field) == Some(Sort::Str) => {
                    terms.insert(field.clone(), self.enc.fresh("spawned", "Str"));
                }
                Effect::Spawn { .. } => {}
                // Armed as the move commits (`Moves::sends`).
                Effect::Schedule { .. } | Effect::ScheduleAt { .. } => {}
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

    /// The dispatches and module calls `action`'s triggers put in flight,
    /// read from the record as it committed, and the actions it schedules.
    pub(super) fn sends(
        &self,
        entity: &str,
        action: &Action,
        params: &BTreeMap<String, Param>,
        changed: &mut BTreeMap<String, String>,
    ) {
        let v = self.enc.v;
        let mut started: Vec<(String, String, Option<String>)> = Vec::new();
        for trigger in &action.triggers {
            let (name, args): (&str, Vec<String>) = if let Some(call) = v.call_of(entity, trigger) {
                let asked = call
                    .asked
                    .iter()
                    .map(|field| Enc::var("post", entity, field))
                    .collect();
                (&call.name, asked)
            } else if let Some(kind) = v.kind_of(entity, action, trigger) {
                let args = kind
                    .fields
                    .iter()
                    .map(|(field, sort)| match &trigger.args[field] {
                        Arg::Param(p) => self.passed(action, params, p, *sort),
                        other => self.arg(entity, other, *sort, "post", &BTreeMap::new()),
                    })
                    .collect();
                (&kind.name, args)
            } else {
                continue;
            };
            let msg = if args.is_empty() {
                format!("mk/{name}")
            } else {
                format!("(mk/{name} {})", args.join(" "))
            };
            let fires = trigger
                .guard
                .as_ref()
                .map(|guard| self.enc.expr(guard, entity, "post"));
            started.push((name.to_string(), msg, fires));
        }
        for effect in &action.effect {
            let Some(kind) = scheduled(effect)
                .filter(|_| false)
                .and_then(|s| v.scheduled_kind(entity, &action.name, s))
            else {
                continue;
            };
            // `schedule_at` arms only when its field holds a timestamp,
            // which the proof does not know.
            let fires = matches!(effect, Effect::ScheduleAt { .. })
                .then(|| self.enc.fresh("armed", "Bool"));
            started.push((kind.name.clone(), format!("mk/{}", kind.name), fires));
        }
        for (name, msg, fires) in started {
            for (part, add) in [
                (
                    "pend",
                    format!("(store {{}} {msg} (+ (select {{}} {msg}) 1))"),
                ),
                ("n", "(+ {} 1)".to_string()),
            ] {
                let key = format!("post.{part}.{name}");
                let now = changed
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| format!("pre.{part}.{name}"));
                let sent = add.replace("{}", &now);
                let next = match &fires {
                    Some(fires) => format!("(ite {fires} {sent} {now})"),
                    None => sent,
                };
                changed.insert(key, next);
            }
        }
    }

    /// The value of `action`'s parameter `name` where a value of `sort` is
    /// read. A parameter declared with no type, or of another sort, is
    /// converted by the kernel; the proof takes any value of `sort`.
    fn passed(
        &self,
        action: &Action,
        params: &BTreeMap<String, Param>,
        name: &str,
        sort: Sort,
    ) -> String {
        let same = self.enc.v.param(action, name).map(Sort::smt) == Some(sort.smt());
        match params.get(name) {
            Some(param) if same => param.term.clone(),
            _ => self.enc.fresh(&format!("p.{name}"), sort.smt()),
        }
    }

    /// The term of an argument read from `entity`'s record in `frame`, or
    /// from `terms` when it holds the variable.
    pub(super) fn arg(
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
        held.unwrap_or_else(|| {
            let reason = match arg {
                Arg::Var(var) if self.enc.v.var(entity, var).is_none() => {
                    self.enc.why_not(entity, var)
                }
                other => format!("a value of another sort ({entity}: {other})"),
            };
            self.enc.unknown_of(reason, sort.smt())
        })
    }
}
