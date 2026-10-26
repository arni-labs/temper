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
//! The moves allow what the joint walk does and more (`moves.rs`), so a
//! proof here holds wherever the walk's model of the kernel does. An
//! `eventually` rule is proved with a measure and helpful moves
//! (`live.rs`).

mod cedar;
mod effects;
mod encode;
mod live;
mod moves;
mod solver;
mod vocab;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Name};

use encode::{Enc, initial, prelude};
pub use live::{Measure, Region};
use moves::Moves;
use solver::{Sat, Z3};
use vocab::Vocab;

/// What an agent gives the proofs: lemmas, and for each `eventually` rule a
/// measure and the regions that say which system move brings it closer.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Hints {
    #[serde(default)]
    pub lemma: Vec<Lemma>,
    #[serde(default)]
    pub measure: Vec<Measure>,
    #[serde(default)]
    pub region: Vec<Region>,
}

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

/// Prove each invariant and `eventually` rule of `automatons` with the hints
/// given for it.
pub fn prove(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    hints: &Hints,
) -> std::io::Result<Vec<RuleProof>> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut rules = Vec::new();
    for automaton in automatons {
        let owner = automaton.automaton.name.as_str();
        for invariant in &automaton.invariants {
            rules.push((
                owner,
                invariant.name.as_str(),
                Rule::Always(&invariant.assert),
            ));
        }
        for liveness in &automaton.liveness {
            if let Some(goal) = &liveness.eventually {
                let rule = Rule::Eventually(goal, &liveness.relies_on);
                rules.push((owner, liveness.name.as_str(), rule));
            }
        }
    }
    // Each rule has its own solver, so the rules are proved side by side.
    std::thread::scope(|scope| {
        let proofs: Vec<_> = rules
            .iter()
            .map(|&(owner, name, rule)| {
                let by_name = &by_name;
                scope.spawn(move || prove_one(by_name, owner, name, rule, hints, policies))
            })
            .collect();
        proofs
            .into_iter()
            .map(|proof| proof.join().expect("a proof thread panicked"))
            .collect()
    })
}

/// A rule: always, at states with no dispatch in flight; or eventually, as
/// the system's moves and the actions it relies on bring it about.
#[derive(Clone, Copy)]
enum Rule<'r> {
    Always(&'r Expr),
    Eventually(&'r Expr, &'r [String]),
}

fn prove_one(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    rule: &str,
    kind: Rule<'_>,
    hints: &Hints,
    policies: Option<&SendAnalysis>,
) -> std::io::Result<RuleProof> {
    let start = Instant::now();
    let expr = match kind {
        Rule::Always(expr) | Rule::Eventually(expr, _) => expr,
    };
    let mut scope = BTreeSet::from([owner.to_string()]);
    expr.for_each_name(&mut |name| {
        if let Name::CrossStatus { entity_type, .. } | Name::CrossField { entity_type, .. } = name {
            if by_name.contains_key(entity_type) {
                scope.insert(entity_type.to_string());
            }
        }
    });
    let lemmas: Vec<&Lemma> = hints.lemma.iter().filter(|l| l.rule == rule).collect();
    let regions: Vec<&Region> = hints.region.iter().filter(|r| r.rule == rule).collect();
    let measure = hints.measure.iter().find(|m| m.rule == rule);
    let texts = lemmas
        .iter()
        .map(|l| l.assert.as_str())
        .chain(regions.iter().map(|r| r.region.as_str()))
        .chain(
            regions
                .iter()
                .flat_map(|r| r.params.values().map(String::as_str)),
        )
        .chain(
            measure
                .iter()
                .flat_map(|m| m.terms.iter().map(String::as_str)),
        );
    let literals: Vec<String> = texts.flat_map(quoted_literals).collect();
    let v = Vocab::new(by_name.clone(), scope.clone(), policies, literals);
    let enc = Enc::new(&v);
    let builder = Moves::new(&enc);
    let moves = builder.all();
    let mut z3 = Z3::start()?;
    z3.send(&prelude(&v))?;
    // The clauses of J, by name, as functions of the frame.
    let clause = |frame: &str| -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Rule::Always(assert) = kind {
            out.push((
                rule.to_string(),
                format!("(=> {frame}.settled {})", enc.expr(assert, owner, frame)),
            ));
        }
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
        let mut text = String::from("(push 1)\n");
        decls
            .iter()
            .chain(&mv.decls)
            .for_each(|d| text.push_str(&format!("{d}\n")));
        for (_, c) in &before {
            text.push_str(&format!("(assert {c})\n"));
        }
        for fact in mv.body() {
            text.push_str(&format!("(assert {fact})\n"));
        }
        obligations.push(check(&mut z3, &v, &mv.label, text, &after, &before)?);
    }
    let mut errors = Vec::new();
    if let Rule::Eventually(goal, relies_on) = kind {
        let live = live::Live {
            owner,
            goal,
            relies_on,
            measure,
            regions,
        };
        live::obligations(
            &mut z3,
            &enc,
            &moves,
            &live,
            &clause,
            &mut obligations,
            &mut errors,
        )?;
    }
    let limits = builder.limits.into_inner().into_iter().collect();
    if let Ok(path) = std::env::var("INDUCT_SCRIPT") {
        std::fs::write(format!("{path}.{rule}.smt2"), &z3.script)?;
    }
    errors.append(&mut z3.errors);
    Ok(RuleProof {
        rule: rule.to_string(),
        scope,
        obligations,
        limits,
        errors,
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

/// At most two distinct dispatches or calls of each kind in flight before
/// the move.
fn few_in_flight(v: &Vocab<'_>) -> String {
    let mut out = String::new();
    for flight in v.flights() {
        let k = flight.name;
        let mut pend = format!("((as const (Array Msg/{k} Int)) 0)");
        for i in 1..=2 {
            out.push_str(&format!(
                "(declare-const few.{k}.{i} Msg/{k})\n(declare-const few.{k}.{i}.n Int)\n(assert (>= few.{k}.{i}.n 0))\n"
            ));
            pend = format!("(store {pend} few.{k}.{i} few.{k}.{i}.n)");
        }
        out.push_str(&format!(
            "(assert (= pre.pend.{k} {pend}))\n(assert (= pre.n.{k} (+ (ite (= few.{k}.1 few.{k}.2) 0 few.{k}.1.n) few.{k}.2.n)))\n"
        ));
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
        for flight in v.flights() {
            out.push(format!("{frame}.pend.{}", flight.name));
        }
    }
    out.extend(v.literals.iter().map(|l| Enc::lit(l)));
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
