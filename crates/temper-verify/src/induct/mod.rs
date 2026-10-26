//! Proof by induction of an app's invariants, for every run.
//!
//! The joint walk checks a rule over the states a bounded walk reaches. This
//! proves it for all of them: a set of clauses `J` (the rule, read at states
//! with nothing in flight, and the lemmas it needs) holds at the start, and
//! every move from a state where `J` holds leads to one where it holds. Each
//! move's obligation is one solver check, whatever the number of states.
//!
//! When an obligation fails, the solver's model is a counterexample to
//! induction: a state where `J` holds and a move from it after which it does
//! not. If that state is reachable, the rule is broken; if not, a lemma that
//! rules it out is missing. Lemmas are SMT-LIB over `$`-framed names (see
//! `encode.rs`), read at both ends of each move.
//!
//! The moves allow more than the kernel does (`moves.rs`), so a proof here
//! holds in the runtime.

mod encode;
mod moves;
mod solver;
mod vocab;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, Initial};
use temper_spec::predicate::{Expr, Name};

use encode::{Enc, int};
use moves::{Move, Moves};
use solver::{Sat, Z3};
use vocab::Vocab;

/// A lemma: a clause the proof holds alongside the rule.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Lemma {
    /// The rule it is for.
    pub rule: String,
    pub name: String,
    /// SMT-LIB over `$`-framed names.
    pub assert: String,
}

/// One obligation's outcome.
#[derive(Debug, Clone)]
pub struct Obligation {
    /// The move, or `start`.
    pub name: String,
    pub took: Duration,
    pub outcome: Outcome,
    /// Whether the move can happen at all from a state where the clauses
    /// hold. One that cannot holds trivially, which a reader should see.
    pub possible: bool,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Holds,
    /// Some clauses fail after it: their names, and the counterexample.
    Fails {
        clauses: Vec<String>,
        cti: String,
    },
    Unknown,
}

/// A rule's proof.
#[derive(Debug, Clone)]
pub struct RuleProof {
    pub rule: String,
    pub scope: BTreeSet<String>,
    pub obligations: Vec<Obligation>,
    /// What the encoding could not express.
    pub limits: Vec<String>,
    /// What the solver refused, such as a lemma naming an unknown kind.
    pub errors: Vec<String>,
    /// Wall time, from reading the specs to the last check.
    pub took: Duration,
}

impl RuleProof {
    pub fn proved(&self) -> bool {
        self.errors.is_empty()
            && self
                .obligations
                .iter()
                .all(|o| matches!(o.outcome, Outcome::Holds))
    }
}

/// Prove each invariant of `automatons` with the lemmas given for it.
pub fn prove(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    lemmas: &[Lemma],
) -> std::io::Result<Vec<RuleProof>> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut out = Vec::new();
    for automaton in automatons {
        for invariant in &automaton.invariants {
            let owner = automaton.automaton.name.as_str();
            let mine: Vec<&Lemma> = lemmas.iter().filter(|l| l.rule == invariant.name).collect();
            out.push(prove_one(
                &by_name,
                owner,
                &invariant.name,
                &invariant.assert,
                &mine,
                policies,
            )?);
        }
    }
    Ok(out)
}

fn prove_one(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    rule: &str,
    assert: &Expr,
    lemmas: &[&Lemma],
    policies: Option<&SendAnalysis>,
) -> std::io::Result<RuleProof> {
    let start = Instant::now();
    let mut scope = BTreeSet::from([owner.to_string()]);
    assert.for_each_name(&mut |name| {
        if let Name::CrossStatus { entity_type, .. } | Name::CrossField { entity_type, .. } = name {
            if by_name.contains_key(entity_type) {
                scope.insert(entity_type.to_string());
            }
        }
    });
    let lemma_literals = lemmas.iter().flat_map(|l| quoted_literals(&l.assert));
    let v = Vocab::new(by_name.clone(), scope.clone(), policies, lemma_literals);
    let enc = Enc::new(&v);
    let builder = Moves::new(&enc);
    let moves = builder.all();
    let mut z3 = Z3::start()?;
    z3.send(&prelude(&v))?;
    // The clauses of J, by name, as functions of the frame.
    let clause = |frame: &str| -> Vec<(String, String)> {
        let mut out = vec![(
            rule.to_string(),
            format!("(=> {frame}.settled {})", enc.expr(assert, owner, frame)),
        )];
        out.extend(
            lemmas
                .iter()
                .map(|l| (l.name.clone(), l.assert.replace('$', &format!("{frame}."))))
                .collect::<Vec<_>>(),
        );
        out
    };
    let mut obligations = Vec::new();
    // The start.
    {
        let at_start = clause("pre");
        let decls = enc.take_decls();
        let mut text = String::from("(push 1)\n");
        decls.iter().for_each(|d| text.push_str(&format!("{d}\n")));
        for fact in initial(&v) {
            text.push_str(&format!("(assert {fact})\n"));
        }
        obligations.push(check(&mut z3, &v, "start", text, &at_start, &at_start)?);
    }
    for mv in &moves {
        let before = clause("pre");
        let after = clause("post");
        let decls = enc.take_decls();
        let Move {
            label,
            decls: own,
            body,
        } = mv;
        let mut text = String::from("(push 1)\n");
        decls
            .iter()
            .chain(own)
            .for_each(|d| text.push_str(&format!("{d}\n")));
        for (_, c) in &before {
            text.push_str(&format!("(assert {c})\n"));
        }
        for fact in body {
            text.push_str(&format!("(assert {fact})\n"));
        }
        obligations.push(check(&mut z3, &v, label, text, &after, &before)?);
    }
    let limits = builder.limits.into_inner().into_iter().collect();
    if let Ok(path) = std::env::var("INDUCT_SCRIPT") {
        std::fs::write(format!("{path}.{rule}.smt2"), &z3.script)?;
    }
    Ok(RuleProof {
        rule: rule.to_string(),
        scope,
        obligations,
        limits,
        errors: std::mem::take(&mut z3.errors),
        took: start.elapsed(),
    })
}

/// Check that `goal` holds given what `text` asserts; on a failure, which
/// clauses fail and the state before (`before` names what to show).
fn check(
    z3: &mut Z3,
    v: &Vocab<'_>,
    name: &str,
    text: String,
    goal: &[(String, String)],
    before: &[(String, String)],
) -> std::io::Result<Obligation> {
    let all: Vec<&str> = goal.iter().map(|(_, c)| c.as_str()).collect();
    z3.send(&text)?;
    // Whether the move can happen: a model is quick to find among states
    // with a few dispatches in flight, and slow or out of reach with
    // quantified lemmas over all of them, so look there first.
    z3.send(&format!("(push 1)\n{}", few_in_flight(v)))?;
    let (few, mut took) = z3.check()?;
    z3.send("(pop 1)\n")?;
    let possible = few == Sat::Sat || {
        let (any, also) = z3.check()?;
        took += also;
        any != Sat::Unsat
    };
    z3.send(&format!("(assert (not (and true {})))\n", all.join(" ")))?;
    let (mut sat, also) = z3.check()?;
    took += also;
    // Quantified lemmas can leave the solver unable to build a model of a
    // failure. Look again among states with a few dispatches of each kind
    // in flight: a failure there is a real counterexample.
    let finite = sat == Sat::Unknown;
    if finite {
        z3.send(&format!("(push 1)\n{}", few_in_flight(v)))?;
        let (again, also) = z3.check()?;
        sat = again;
        took += also;
        if sat != Sat::Sat {
            z3.send("(pop 1)\n")?;
        }
    }
    let outcome = match sat {
        Sat::Unsat | Sat::Unknown if finite => Outcome::Unknown,
        Sat::Unsat => Outcome::Holds,
        Sat::Unknown => Outcome::Unknown,
        Sat::Sat => {
            let cti = z3.values(&shown(v, before, goal))?;
            if finite {
                z3.send("(pop 1)\n")?;
            }
            z3.send("(pop 1)\n(push 1)\n")?;
            z3.send(&text)?;
            if finite {
                z3.send(&few_in_flight(v))?;
            }
            let mut clauses = Vec::new();
            for (clause_name, c) in goal {
                z3.send(&format!("(push 1)\n(assert (not {c}))\n"))?;
                if z3.check()?.0 != Sat::Unsat {
                    clauses.push(clause_name.clone());
                }
                z3.send("(pop 1)\n")?;
            }
            Outcome::Fails { clauses, cti }
        }
    };
    z3.send("(pop 1)\n")?;
    Ok(Obligation {
        name: name.to_string(),
        took,
        outcome,
        possible,
    })
}

/// At most two distinct dispatches of each kind in flight before the move.
fn few_in_flight(v: &Vocab<'_>) -> String {
    let mut out = String::new();
    for kind in &v.kinds {
        let k = &kind.name;
        let mut pend = format!("((as const (Array Msg/{k} Int)) 0)");
        for i in 1..=2 {
            out.push_str(&format!(
                "(declare-const few.{k}.{i} Msg/{k})\n(declare-const few.{k}.{i}.n Int)\n(assert (>= few.{k}.{i}.n 0))\n"
            ));
            pend = format!("(store {pend} few.{k}.{i} few.{k}.{i}.n)");
        }
        out.push_str(&format!("(assert (= pre.pend.{k} {pend}))\n"));
    }
    out
}

/// The terms a counterexample shows: the records' statuses, the variables
/// the clauses read, and the dispatches in flight, before and after.
fn shown(v: &Vocab<'_>, before: &[(String, String)], after: &[(String, String)]) -> Vec<String> {
    let text: String = before
        .iter()
        .chain(after)
        .map(|(_, c)| c.as_str())
        .collect();
    let mut out = Vec::new();
    for frame in ["pre", "post"] {
        for (entity, vars) in &v.vars {
            out.push(Enc::status(frame, entity));
            for var in vars.keys() {
                let name = Enc::var(frame, entity, var);
                let pre = Enc::var("pre", entity, var);
                let post = Enc::var("post", entity, var);
                if text.contains(&format!("{pre} "))
                    || text.contains(&format!("{pre})"))
                    || text.contains(&format!("{post} "))
                    || text.contains(&format!("{post})"))
                {
                    out.push(name);
                }
            }
        }
        for kind in &v.kinds {
            out.push(format!("{frame}.pend.{}", kind.name));
        }
    }
    out.extend(v.literals.iter().map(|l| Enc::lit(l)));
    out
}

/// The declarations every obligation shares.
fn prelude(v: &Vocab<'_>) -> String {
    let mut out = String::from("(declare-sort Str 0)\n");
    let literals: Vec<String> = v.literals.iter().map(|l| Enc::lit(l)).collect();
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
    for kind in &v.kinds {
        let fields: Vec<String> = kind
            .fields
            .iter()
            .map(|(f, s)| format!("({}/{f} {})", kind.name, s.smt()))
            .collect();
        out.push_str(&format!(
            "(declare-datatypes ((Msg/{0} 0)) (((mk/{0} {1}))))\n",
            kind.name,
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
        for kind in &v.kinds {
            let k = &kind.name;
            out.push_str(&format!(
                "(declare-const {frame}.pend.{k} (Array Msg/{k} Int))\n"
            ));
            out.push_str(&format!(
                "(define-fun {frame}.has.{k} ((m Msg/{k})) Bool (> (select {frame}.pend.{k} m) 0))\n"
            ));
            settled.push(format!(
                "(= {frame}.pend.{k} ((as const (Array Msg/{k} Int)) 0))"
            ));
        }
        out.push_str(&format!(
            "(define-fun {frame}.settled () Bool (and true {}))\n",
            settled.join(" ")
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
            if let vocab::Sort::Int { counter: true } = sort {
                out.push_str(&format!(
                    "(assert (>= {} 0))\n",
                    Enc::var("pre", entity, var)
                ));
            }
        }
    }
    for kind in &v.kinds {
        let k = &kind.name;
        out.push_str(&format!(
            "(assert (forall ((m Msg/{k})) (>= (select pre.pend.{k} m) 0)))\n"
        ));
    }
    out
}

/// The records as they start, with nothing in flight.
fn initial(v: &Vocab<'_>) -> Vec<String> {
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
    for kind in &v.kinds {
        let k = &kind.name;
        out.push(format!(
            "(= pre.pend.{k} ((as const (Array Msg/{k} Int)) 0))"
        ));
    }
    out
}

/// The string literals a lemma names, as `|'x'|`.
fn quoted_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("|'") {
        let after = &rest[at + 2..];
        let Some(end) = after.find("'|") else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 2..];
    }
    out
}
