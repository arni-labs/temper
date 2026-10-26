//! The declarations every obligation shares, and the initial states.

use std::collections::BTreeSet;

use temper_spec::automaton::Initial;

use super::encode::{Enc, int};
use super::vocab::{Sort, Vocab};

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
