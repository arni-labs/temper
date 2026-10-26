//! Running a rule's obligations: each is written out as text first, then
//! checked by one of a few z3 processes that each hold the vocabulary.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::encode::Enc;
use super::solver::{Sat, Z3};
use super::vocab::Vocab;
use super::{Obligation, Outcome};

/// One obligation written out: what it assumes, what must follow, and what
/// a counterexample shows.
pub(super) struct Ask {
    pub(super) name: String,
    /// Starts with `(push 1)`: its declarations and what it assumes.
    pub(super) text: String,
    pub(super) goal: Vec<(String, String)>,
    pub(super) show: Vec<String>,
}

impl Ask {
    pub(super) fn new(
        v: &Vocab<'_>,
        name: &str,
        text: String,
        goal: Vec<(String, String)>,
        before: &[(String, String)],
    ) -> Self {
        let show = shown(v, before, &goal);
        Self {
            name: name.to_string(),
            text,
            goal,
            show,
        }
    }
}

/// Check `asks` in their order over `solvers` z3 processes that each hold
/// `prelude`. A rule is not proved once one obligation does not hold, so
/// the solvers stop there, unless all it breaks is `guessed` clauses, which
/// the prover drops and tries again without; the obligations checked are
/// returned in order, with what the solvers refused.
pub(super) fn run(
    rule: &str,
    prelude: &str,
    few: &str,
    literals: &[String],
    asks: &[Ask],
    solvers: usize,
    guessed: &BTreeSet<String>,
) -> std::io::Result<(Vec<Obligation>, Vec<String>)> {
    let ends = |o: &Obligation| match &o.outcome {
        Outcome::Holds => false,
        Outcome::Fails { clauses, .. } => {
            clauses.is_empty() || clauses.iter().any(|c| !guessed.contains(c))
        }
        Outcome::Unknown => true,
    };
    let ends = &ends;
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (next, stop) = (&next, &stop);
    let mut done = Vec::new();
    let mut errors = Vec::new();
    std::thread::scope(|scope| -> std::io::Result<()> {
        let workers: Vec<_> = (0..solvers)
            .map(|i| {
                scope.spawn(
                    move || -> std::io::Result<(Vec<(usize, Obligation)>, Vec<String>)> {
                        let mut z3 = Z3::start()?;
                        z3.send(prelude)?;
                        z3.send(few)?;
                        let mut checked = Vec::new();
                        while !stop.load(Ordering::Relaxed) {
                            let at = next.fetch_add(1, Ordering::Relaxed);
                            let Some(ask) = asks.get(at) else {
                                break;
                            };
                            let obligation = check(&mut z3, literals, ask)?;
                            if ends(&obligation) {
                                stop.store(true, Ordering::Relaxed);
                            }
                            checked.push((at, obligation));
                        }
                        if let (Ok(path), Some(script)) =
                            (std::env::var("INDUCT_SCRIPT"), &z3.script)
                        {
                            std::fs::write(format!("{path}.{rule}.{i}.smt2"), script)?;
                        }
                        Ok((checked, std::mem::take(&mut z3.errors)))
                    },
                )
            })
            .collect();
        for worker in workers {
            let (checked, refused) = worker.join().expect("a solver thread panicked")?;
            done.extend(checked);
            for error in refused {
                // Every solver reads the same lemmas and refuses the same,
                // at the line of its own script.
                let error = match error.split_once(": ") {
                    Some((at, said)) if at.contains(" column ") => format!("(error \"{said}"),
                    _ => error,
                };
                if !errors.contains(&error) {
                    errors.push(error);
                }
            }
        }
        Ok(())
    })?;
    done.sort_by_key(|(at, _)| *at);
    // Solvers already checking when one fails may fail too; the first is
    // the one reported.
    let mut out: Vec<Obligation> = done.into_iter().map(|(_, o)| o).collect();
    if let Some(first) = out.iter().position(ends) {
        out.truncate(first + 1);
    }
    Ok((out, errors))
}

/// Check that `ask`'s goal holds given what it assumes; on a failure, which
/// clauses fail and the state it shows.
fn check(z3: &mut Z3, literals: &[String], ask: &Ask) -> std::io::Result<Obligation> {
    let show = [ask.show.as_slice(), literals].concat();
    let all: Vec<&str> = ask.goal.iter().map(|(_, c)| c.as_str()).collect();
    let negated = format!("(assert (not (and true {})))\n", all.join(" "));
    z3.send(&ask.text)?;
    // Over all states, quantified lemmas can leave the solver searching for
    // a model until its timeout. Among states with a few dispatches of each
    // kind in flight one is quick to find: first whether the move can
    // happen, then a failure, which is a real counterexample.
    let quantified = |t: &str| t.contains("(forall ") || t.contains("(exists ");
    let few_first = quantified(&ask.text) || all.iter().any(|c| quantified(c));
    let (mut some, mut took) = (Sat::Unknown, Duration::ZERO);
    if few_first {
        z3.send("(push 1)\n(assert few!)\n")?;
        (some, took) = z3.check()?;
    }
    if some == Sat::Sat {
        z3.send(&negated)?;
        let (fails, also) = z3.check()?;
        took += also;
        if fails == Sat::Sat {
            let cti = z3.values(&show)?;
            z3.send("(pop 1)\n(push 1)\n(assert few!)\n")?;
            let clauses = failing(z3, &ask.goal)?;
            z3.send("(pop 1)\n(pop 1)\n")?;
            return Ok(Obligation {
                name: ask.name.clone(),
                took,
                outcome: Outcome::Fails { clauses, cti },
                possible: true,
            });
        }
    }
    if few_first {
        z3.send("(pop 1)\n")?;
    }
    let possible = some == Sat::Sat || {
        let (any, also) = z3.check()?;
        took += also;
        any != Sat::Unsat
    };
    z3.send(&negated)?;
    let (sat, also) = z3.check()?;
    took += also;
    let outcome = match sat {
        Sat::Unsat => Outcome::Holds,
        Sat::Unknown => Outcome::Unknown,
        Sat::Sat => {
            let cti = z3.values(&show)?;
            z3.send("(pop 1)\n")?;
            z3.send(&ask.text)?;
            let clauses = failing(z3, &ask.goal)?;
            Outcome::Fails { clauses, cti }
        }
    };
    z3.send("(pop 1)\n")?;
    Ok(Obligation {
        name: ask.name.clone(),
        took,
        outcome,
        possible,
    })
}

/// The clauses of `goal` that fail on their own, given what is asserted.
fn failing(z3: &mut Z3, goal: &[(String, String)]) -> std::io::Result<Vec<String>> {
    let mut clauses = Vec::new();
    for (clause_name, c) in goal {
        z3.send(&format!("(push 1)\n(assert (not {c}))\n"))?;
        if z3.check()?.0 != Sat::Unsat {
            clauses.push(clause_name.clone());
        }
        z3.send("(pop 1)\n")?;
    }
    Ok(clauses)
}

/// `few!`: at most two distinct dispatches or calls of each kind in flight
/// before the move.
pub(super) fn few_in_flight(v: &Vocab<'_>) -> String {
    let mut out = String::new();
    let mut few = Vec::new();
    for flight in v.flights() {
        let k = flight.name;
        let mut pend = format!("((as const (Array Msg/{k} Int)) 0)");
        for i in 1..=2 {
            out.push_str(&format!(
                "(declare-const few.{k}.{i} Msg/{k})\n(declare-const few.{k}.{i}.n Int)\n"
            ));
            few.push(format!("(>= few.{k}.{i}.n 0)"));
            pend = format!("(store {pend} few.{k}.{i} few.{k}.{i}.n)");
        }
        few.push(format!("(= pre.pend.{k} {pend})"));
        few.push(format!(
            "(= pre.n.{k} (+ (ite (= few.{k}.1 few.{k}.2) 0 few.{k}.1.n) few.{k}.2.n))"
        ));
    }
    out.push_str(&format!(
        "(define-fun few! () Bool (and true {}))\n",
        few.join(" ")
    ));
    out
}

/// The terms a counterexample shows: the records' statuses, the variables
/// the clauses read, and the dispatches in flight, before and after; the
/// literals are added when it is checked.
fn shown(v: &Vocab<'_>, before: &[(String, String)], after: &[(String, String)]) -> Vec<String> {
    let named: BTreeSet<&str> = before
        .iter()
        .chain(after)
        .flat_map(|(_, c)| c.split([' ', '(', ')']))
        .collect();
    let mut out = Vec::new();
    for frame in ["pre", "post"] {
        for (entity, vars) in &v.vars {
            out.push(Enc::status(frame, entity));
            for var in vars.keys() {
                let read = |f: &str| named.contains(Enc::var(f, entity, var).as_str());
                if read("pre") || read("post") {
                    out.push(Enc::var(frame, entity, var));
                }
            }
        }
        for name in v
            .kinds
            .iter()
            .map(|k| &k.name)
            .chain(v.calls.iter().map(|c| &c.name))
        {
            out.push(format!("{frame}.pend.{name}"));
        }
    }
    out
}
