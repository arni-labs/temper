//! What a move writes: the record after its action, read in order from the
//! parameters it is passed and its effects, and the dispatches and module
//! calls its triggers put in flight.

use std::collections::BTreeMap;

use temper_spec::automaton::Action;
use temper_spec::predicate::{Arg, AssignOp, Effect};

use super::encode::Enc;
use super::moves::{Moves, Param};
use super::vocab::Sort;

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
                    let value = match value {
                        Arg::Param(p) => self.passed(action, params, p, sort),
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

    /// The dispatches and module calls `action`'s triggers put in flight,
    /// read from the record as it committed.
    pub(super) fn sends(
        &self,
        entity: &str,
        action: &Action,
        params: &BTreeMap<String, Param>,
        changed: &mut BTreeMap<String, String>,
    ) {
        let v = self.enc.v;
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
        held.unwrap_or_else(|| self.enc.fresh("h", sort.smt()))
    }
}
