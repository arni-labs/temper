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
use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Initial;
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
    /// Why each unknown since the last `take_unheld` was made up.
    unheld: RefCell<BTreeSet<String>>,
}

impl<'v, 'a> Enc<'v, 'a> {
    pub(super) fn new(v: &'v Vocab<'a>) -> Self {
        Self {
            v,
            fresh: RefCell::new((0, Vec::new())),
            unheld: RefCell::default(),
        }
    }

    /// An unknown condition, made up because the proof does not hold what
    /// `reason` names.
    fn unknown(&self, reason: String) -> String {
        self.unknown_of(reason, "Bool")
    }

    /// An unknown value of `sort`, made up because the proof does not hold
    /// what `reason` names.
    pub(super) fn unknown_of(&self, reason: String, sort: &str) -> String {
        self.unheld.borrow_mut().insert(reason);
        self.fresh("h", sort)
    }

    /// Why the unknowns made up since the last call were made up.
    pub(super) fn take_unheld(&self) -> BTreeSet<String> {
        std::mem::take(&mut self.unheld.borrow_mut())
    }

    /// Why `owner`'s variable `name` is not held: a list, or not one of its
    /// variables.
    pub(super) fn why_not(&self, owner: &str, name: &str) -> String {
        let declared = self
            .v
            .automaton(owner)
            .state
            .iter()
            .any(|var| var.name == name);
        if declared {
            format!("a list ({owner}.{name})")
        } else {
            format!("a name {owner} does not declare ({name})")
        }
    }

    /// The declarations of the unknowns made up since the last call.
    pub(super) fn take_decls(&self) -> Vec<String> {
        std::mem::take(&mut self.fresh.borrow_mut().1)
    }

    /// A fresh name for a `let` binding, declared nowhere.
    pub(super) fn local(&self) -> String {
        let mut fresh = self.fresh.borrow_mut();
        fresh.0 += 1;
        format!("l!{}", fresh.0)
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
                Some(_) => self.unknown(format!("a field read as a condition ({owner}.{name})")),
                None => self.unknown(self.why_not(owner, name)),
            },
            Expr::Compare { lhs, op, rhs } => {
                let terms = self
                    .operand(lhs, owner, frame)
                    .and_then(|l| Ok((l, self.operand(rhs, owner, frame)?)));
                match terms {
                    Ok((l, r)) => self.compare(l, *op, r).unwrap_or_else(|| {
                        self.unknown(format!(
                            "a comparison of mixed sorts or by order on strings ({expr})"
                        ))
                    }),
                    Err(reason) => self.unknown(reason),
                }
            }
            Expr::In {
                value,
                set: Set::List(items),
                negated,
            } => {
                let value = match self.operand(value, owner, frame) {
                    Ok(value) => value,
                    Err(reason) => return self.unknown(reason),
                };
                let mut parts = Vec::new();
                for item in items {
                    let one = self.compare(self.copy(&value), CmpOp::Eq, Term::Lit(item.clone()));
                    parts.push(one.unwrap_or_else(|| {
                        self.unknown(format!("a literal of another sort in a list ({expr})"))
                    }));
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
            Expr::In { .. } => self.unknown(format!("membership in a list field ({expr})")),
            Expr::Empty(name) => match self.v.var(owner, name) {
                Some(Sort::Str) => {
                    format!("(= {} {})", Self::var(frame, owner, name), Self::lit(""))
                }
                _ => self.unknown(self.why_not(owner, name)),
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

    /// An operand's term, or why the proof does not hold it.
    fn operand(&self, operand: &Operand, owner: &str, frame: &str) -> Result<Term, String> {
        let same_id = |id_field: &str| matches!(id_field, "Id" | "id");
        match operand {
            Operand::Status => Ok(Term::Status(owner.to_string(), Self::status(frame, owner))),
            Operand::Var(name) => self
                .v
                .var(owner, name)
                .map(|sort| Term::Val(Self::var(frame, owner, name), sort))
                .ok_or_else(|| self.why_not(owner, name)),
            Operand::Len(name) => Err(format!("the length of a list ({owner}.{name})")),
            Operand::CrossStatus {
                entity_type,
                id_field,
            } if same_id(id_field) && self.v.scope.contains(entity_type) => Ok(Term::Status(
                entity_type.clone(),
                Self::status(frame, entity_type),
            )),
            Operand::CrossField {
                entity_type,
                id_field,
                field,
            } if same_id(id_field) && self.v.scope.contains(entity_type) => self
                .v
                .var(entity_type, field)
                .map(|sort| Term::Val(Self::var(frame, entity_type, field), sort))
                .ok_or_else(|| self.why_not(entity_type, field)),
            Operand::CrossStatus {
                entity_type,
                id_field,
            }
            | Operand::CrossField {
                entity_type,
                id_field,
                ..
            } => Err(if same_id(id_field) {
                format!("a record of a type the specs do not define ({entity_type})")
            } else {
                format!("a record found through a field ({entity_type}[{id_field}])")
            }),
            Operand::Param(name) => Err(format!("a parameter of a move (params.{name})")),
            Operand::Lit(l) => Ok(Term::Lit(l.clone())),
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
        for flight in self.v.flights() {
            for part in ["pend", "n"] {
                let name = format!("{to}.{part}.{}", flight.name);
                out.push(match changed.get(&name) {
                    Some(term) => format!("(= {name} {term})"),
                    None => format!("(= {name} {from}.{part}.{})", flight.name),
                });
            }
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

/// The declarations every obligation shares.
pub(super) fn prelude(v: &Vocab<'_>, literals: &BTreeSet<String>) -> String {
    let mut out = String::from("(declare-sort Str 0)\n");
    let literals: Vec<String> = literals.iter().map(|l| Enc::lit(l)).collect();
    for l in &literals {
        out.push_str(&format!("(declare-const {l} Str)\n"));
    }
    if literals.len() > 1 {
        out.push_str(&format!("(assert (distinct {}))\n", literals.join(" ")));
    }
    for entity in &v.scope {
        for (i, state) in v.automaton(entity).automaton.states.iter().enumerate() {
            out.push_str(&format!(
                "(define-fun {} () Int {i})\n",
                Enc::state(entity, state)
            ));
        }
    }
    for flight in v.flights() {
        let fields: Vec<String> = flight
            .fields
            .iter()
            .map(|(f, s)| format!("({}/{f} {})", flight.name, s.smt()))
            .collect();
        out.push_str(&format!(
            "(declare-datatypes ((Msg/{0} 0)) (((mk/{0} {1}))))\n",
            flight.name,
            fields.join(" ")
        ));
    }
    for frame in ["pre", "post"] {
        let mut settled = Vec::new();
        for entity in &v.scope {
            out.push_str(&format!(
                "(declare-const {} Int)\n",
                Enc::status(frame, entity)
            ));
            for (var, sort) in &v.vars[entity] {
                out.push_str(&format!(
                    "(declare-const {} {})\n",
                    Enc::var(frame, entity, var),
                    sort.smt()
                ));
            }
        }
        let mut quiet = Vec::new();
        for flight in v.flights() {
            let k = flight.name;
            out.push_str(&format!(
                "(declare-const {frame}.pend.{k} (Array Msg/{k} Int))\n(declare-const {frame}.n.{k} Int)\n"
            ));
            out.push_str(&format!(
                "(define-fun {frame}.has.{k} ((m Msg/{k})) Bool (> (select {frame}.pend.{k} m) 0))\n"
            ));
            let kind = v.kinds.iter().find(|kind| kind.name == k);
            if kind.is_some_and(|kind| kind.scheduled) {
                continue;
            }
            if kind.is_some() {
                settled.push(format!(
                    "(= {frame}.pend.{k} ((as const (Array Msg/{k} Int)) 0))"
                ));
            }
            quiet.push(format!("(= {frame}.n.{k} 0)"));
        }
        // An invariant is read where no dispatch is in flight; an
        // `eventually` where nothing is, calls included (the joint walk's
        // settled state). A scheduled action may wait as long as a state
        // timer, so a record waiting for one is both.
        out.push_str(&format!(
            "(define-fun {frame}.settled () Bool (and true {}))\n(define-fun {frame}.quiet () Bool (and true {}))\n",
            settled.join(" "),
            quiet.join(" ")
        ));
    }
    // What every state the proof starts a move from satisfies.
    for entity in &v.scope {
        let states = v.automaton(entity).automaton.states.len();
        let status = Enc::status("pre", entity);
        out.push_str(&format!(
            "(assert (and (<= 0 {status}) (< {status} {states})))\n"
        ));
        for (var, sort) in &v.vars[entity] {
            if let Sort::Int { counter: true } = sort {
                out.push_str(&format!(
                    "(assert (>= {} 0))\n",
                    Enc::var("pre", entity, var)
                ));
            }
        }
    }
    for flight in v.flights() {
        let k = flight.name;
        out.push_str(&format!(
            "(assert (forall ((m Msg/{k})) (>= (select pre.pend.{k} m) 0)))\n"
        ));
    }
    out
}

/// What follows from `n.K` being how many of kind K are in flight before
/// the move: the sum of its counts, which SMT-LIB cannot write. Only the
/// liveness obligations read the counts.
pub(super) fn counted(v: &Vocab<'_>) -> String {
    let mut out = String::new();
    for flight in v.flights() {
        let k = flight.name;
        out.push_str(&format!(
            "(assert (>= pre.n.{k} 0))\n(assert (= (= pre.n.{k} 0) (= pre.pend.{k} ((as const (Array Msg/{k} Int)) 0))))\n"
        ));
        out.push_str(&format!(
            "(assert (forall ((m Msg/{k})) (<= (select pre.pend.{k} m) pre.n.{k})))\n"
        ));
    }
    out
}

/// The records as they start, with nothing in flight.
pub(super) fn initial(v: &Vocab<'_>) -> Vec<String> {
    let mut out = Vec::new();
    for entity in &v.scope {
        let automaton = v.automaton(entity);
        out.push(format!(
            "(= {} {})",
            Enc::status("pre", entity),
            Enc::state(entity, &automaton.automaton.initial)
        ));
        for var in &automaton.state {
            let value = match &var.initial {
                Initial::String(s) => Enc::lit(s),
                Initial::Bool(b) => b.to_string(),
                Initial::Counter(n) => n.to_string(),
                Initial::Int(n) => int(*n),
                Initial::List(_) => continue,
            };
            out.push(format!(
                "(= {} {value})",
                Enc::var("pre", entity, &var.name)
            ));
        }
    }
    for flight in v.flights() {
        let k = flight.name;
        out.push(format!(
            "(= pre.pend.{k} ((as const (Array Msg/{k} Int)) 0))"
        ));
        out.push(format!("(= pre.n.{k} 0)"));
    }
    out
}
