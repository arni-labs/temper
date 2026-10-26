//! Predicates and record terms as SMT-LIB, over a frame: `pre` for the state
//! a move starts from, `post` for the state it leads to.
//!
//! A record's variable is `<frame>.<Entity>.<var>`, its status
//! `<frame>.<Entity>.status` (an integer naming `<Entity>/<State>`), a string
//! literal `'x'` is `|'x'|`, and the dispatches of kind `K` in flight are
//! `<frame>.pend.K`, a count for each value of `Msg/K`. What the proof does
//! not hold (a list, a record found through a field) reads as a fresh
//! unknown, which only weakens what it assumes and strengthens what it must
//! show.

use std::cell::RefCell;
use std::collections::BTreeMap;

use temper_spec::predicate::{CmpOp, Expr, Literal, Operand, Set};

use super::vocab::{Sort, Vocab};

/// An operand's term.
enum Term {
    /// A record's status: its entity and term.
    Status(String, String),
    Val(String, Sort),
    Lit(Literal),
}

/// Writes SMT-LIB for one obligation, declaring the unknowns it makes up.
pub(super) struct Enc<'v, 'a> {
    pub(super) v: &'v Vocab<'a>,
    fresh: RefCell<(usize, Vec<String>)>,
}

impl<'v, 'a> Enc<'v, 'a> {
    pub(super) fn new(v: &'v Vocab<'a>) -> Self {
        Self {
            v,
            fresh: RefCell::new((0, Vec::new())),
        }
    }

    /// The declarations of the unknowns made up since the last call.
    pub(super) fn take_decls(&self) -> Vec<String> {
        std::mem::take(&mut self.fresh.borrow_mut().1)
    }

    /// A fresh constant of `sort`, declared with the obligation.
    pub(super) fn fresh(&self, prefix: &str, sort: &str) -> String {
        let mut fresh = self.fresh.borrow_mut();
        fresh.0 += 1;
        let name = format!("{prefix}!{}", fresh.0);
        fresh.1.push(format!("(declare-const {name} {sort})"));
        name
    }

    pub(super) fn var(frame: &str, entity: &str, var: &str) -> String {
        format!("{frame}.{entity}.{var}")
    }

    pub(super) fn status(frame: &str, entity: &str) -> String {
        format!("{frame}.{entity}.status")
    }

    pub(super) fn state(entity: &str, state: &str) -> String {
        format!("{entity}/{state}")
    }

    pub(super) fn lit(text: &str) -> String {
        if text.contains(['|', '\\']) {
            let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
            format!("|'#{hex}'|")
        } else {
            format!("|'{text}'|")
        }
    }

    /// A literal as a value of `sort`, when it is one.
    pub(super) fn literal(literal: &Literal, sort: Sort) -> Option<String> {
        match (literal, sort) {
            (Literal::Str(s), Sort::Str) => Some(Self::lit(s)),
            (Literal::Null, Sort::Str) => Some(Self::lit("")),
            (Literal::Int(n), Sort::Int { .. }) => Some(int(*n)),
            (Literal::Bool(b), Sort::Bool) => Some(b.to_string()),
            _ => None,
        }
    }

    /// `expr`, read on `owner`'s record in `frame`.
    pub(super) fn expr(&self, expr: &Expr, owner: &str, frame: &str) -> String {
        match expr {
            Expr::Const(b) => b.to_string(),
            Expr::Not(e) => format!("(not {})", self.expr(e, owner, frame)),
            Expr::And(es) => self.join("and", es, owner, frame),
            Expr::Or(es) => self.join("or", es, owner, frame),
            Expr::Implies(a, b) => format!(
                "(=> {} {})",
                self.expr(a, owner, frame),
                self.expr(b, owner, frame)
            ),
            Expr::Var(name) => match self.v.var(owner, name) {
                Some(Sort::Bool) => Self::var(frame, owner, name),
                _ => self.fresh("h", "Bool"),
            },
            Expr::Compare { lhs, op, rhs } => {
                let (Some(l), Some(r)) = (
                    self.operand(lhs, owner, frame),
                    self.operand(rhs, owner, frame),
                ) else {
                    return self.fresh("h", "Bool");
                };
                self.compare(l, *op, r)
                    .unwrap_or_else(|| self.fresh("h", "Bool"))
            }
            Expr::In {
                value,
                set: Set::List(items),
                negated,
            } => {
                let Some(value) = self.operand(value, owner, frame) else {
                    return self.fresh("h", "Bool");
                };
                let mut parts = Vec::new();
                for item in items {
                    let one = self.compare(self.copy(&value), CmpOp::Eq, Term::Lit(item.clone()));
                    parts.push(one.unwrap_or_else(|| self.fresh("h", "Bool")));
                }
                let any = match parts.len() {
                    0 => "false".to_string(),
                    _ => format!("(or false {})", parts.join(" ")),
                };
                if *negated {
                    format!("(not {any})")
                } else {
                    any
                }
            }
            Expr::In { .. } => self.fresh("h", "Bool"),
            Expr::Empty(name) => match self.v.var(owner, name) {
                Some(Sort::Str) => {
                    format!("(= {} {})", Self::var(frame, owner, name), Self::lit(""))
                }
                _ => self.fresh("h", "Bool"),
            },
        }
    }

    fn join(&self, op: &str, es: &[Expr], owner: &str, frame: &str) -> String {
        let parts: Vec<String> = es.iter().map(|e| self.expr(e, owner, frame)).collect();
        format!("({op} {})", parts.join(" "))
    }

    fn copy(&self, term: &Term) -> Term {
        match term {
            Term::Status(e, t) => Term::Status(e.clone(), t.clone()),
            Term::Val(t, s) => Term::Val(t.clone(), *s),
            Term::Lit(l) => Term::Lit(l.clone()),
        }
    }

    fn operand(&self, operand: &Operand, owner: &str, frame: &str) -> Option<Term> {
        let same_id = |id_field: &str| matches!(id_field, "Id" | "id");
        match operand {
            Operand::Status => Some(Term::Status(owner.to_string(), Self::status(frame, owner))),
            Operand::Var(name) => self
                .v
                .var(owner, name)
                .map(|sort| Term::Val(Self::var(frame, owner, name), sort)),
            Operand::Len(_) => None,
            Operand::CrossStatus {
                entity_type,
                id_field,
            } if same_id(id_field) && self.v.scope.contains(entity_type) => Some(Term::Status(
                entity_type.clone(),
                Self::status(frame, entity_type),
            )),
            Operand::CrossField {
                entity_type,
                id_field,
                field,
            } if same_id(id_field) => self
                .v
                .var(entity_type, field)
                .map(|sort| Term::Val(Self::var(frame, entity_type, field), sort)),
            Operand::CrossStatus { .. } | Operand::CrossField { .. } => None,
            Operand::Lit(l) => Some(Term::Lit(l.clone())),
        }
    }

    fn compare(&self, lhs: Term, op: CmpOp, rhs: Term) -> Option<String> {
        let eq_only = |eq: String| match op {
            CmpOp::Eq => Some(eq),
            CmpOp::Ne => Some(format!("(not {eq})")),
            _ => None,
        };
        match (lhs, rhs) {
            (Term::Status(entity, t), Term::Lit(Literal::Str(s)))
            | (Term::Lit(Literal::Str(s)), Term::Status(entity, t)) => {
                let named = self.v.automaton(&entity).automaton.states.contains(&s);
                if named {
                    eq_only(format!("(= {t} {})", Self::state(&entity, &s)))
                } else {
                    eq_only("false".to_string())
                }
            }
            (Term::Status(a, ta), Term::Status(b, tb)) if a == b => {
                eq_only(format!("(= {ta} {tb})"))
            }
            (Term::Val(t, sort), Term::Lit(l)) => {
                let l = Self::literal(&l, sort)?;
                self.relate(&t, op, &l, sort)
            }
            (Term::Lit(l), Term::Val(t, sort)) => {
                let l = Self::literal(&l, sort)?;
                self.relate(&l, op, &t, sort)
            }
            (Term::Val(a, sa), Term::Val(b, sb)) if sa.smt() == sb.smt() => {
                self.relate(&a, op, &b, sa)
            }
            (Term::Lit(a), Term::Lit(b)) => eq_only((a == b).to_string()),
            _ => None,
        }
    }

    fn relate(&self, a: &str, op: CmpOp, b: &str, sort: Sort) -> Option<String> {
        match (op, sort) {
            (CmpOp::Eq, _) => Some(format!("(= {a} {b})")),
            (CmpOp::Ne, _) => Some(format!("(not (= {a} {b}))")),
            (CmpOp::Lt, Sort::Int { .. }) => Some(format!("(< {a} {b})")),
            (CmpOp::Le, Sort::Int { .. }) => Some(format!("(<= {a} {b})")),
            (CmpOp::Gt, Sort::Int { .. }) => Some(format!("(> {a} {b})")),
            (CmpOp::Ge, Sort::Int { .. }) => Some(format!("(>= {a} {b})")),
            _ => None,
        }
    }

    /// Every record and dispatch count in `frame` the same as in `from`,
    /// except what `changed` names.
    pub(super) fn frame_rest(
        &self,
        from: &str,
        to: &str,
        changed: &BTreeMap<String, String>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (entity, vars) in &self.v.vars {
            let status = Self::status(to, entity);
            out.push(match changed.get(&status) {
                Some(term) => format!("(= {status} {term})"),
                None => format!("(= {status} {})", Self::status(from, entity)),
            });
            for var in vars.keys() {
                let name = Self::var(to, entity, var);
                out.push(match changed.get(&name) {
                    Some(term) => format!("(= {name} {term})"),
                    None => format!("(= {name} {})", Self::var(from, entity, var)),
                });
            }
        }
        for kind in &self.v.kinds {
            let name = format!("{to}.pend.{}", kind.name);
            out.push(match changed.get(&name) {
                Some(term) => format!("(= {name} {term})"),
                None => format!("(= {name} {from}.pend.{})", kind.name),
            });
        }
        out
    }
}

pub(super) fn int(n: i64) -> String {
    if n < 0 {
        format!("(- {})", n.unsigned_abs())
    } else {
        n.to_string()
    }
}
