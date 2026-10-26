//! A rule's obligations, each written out as text to check (`pool.rs`
//! runs them), and how one is checked.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::encode::Enc;
use super::solver::{Answer, Z3};
use super::vocab::Vocab;
use super::{Obligation, Outcome};

/// One obligation written out: what it assumes, what must follow, and what
/// a counterexample shows.
pub(super) struct Ask {
    pub(super) name: String,
    /// Starts with `(push 1)`: its declarations and what it assumes.
    pub(super) text: String,
    /// Shared by the asks that check the same clauses.
    pub(super) goal: Arc<[(String, String)]>,
    pub(super) show: Arc<[String]>,
    /// Whether it assumes the clauses the proof holds before the move.
    pub(super) assumes: bool,
    /// A general move's ([`super::general`]): the asks of the moves it
    /// stands for, checked instead when it does not hold.
    pub(super) members: Vec<usize>,
    /// Whether a clause it reads is quantified.
    pub(super) quantified: bool,
}

impl Ask {
    pub(super) fn new(
        name: &str,
        text: String,
        goal: Arc<[(String, String)]>,
        reads: &Reads,
    ) -> Self {
        Self {
            name: name.to_string(),
            quantified: reads.quantified || quantified(&text),
            text,
            goal,
            show: Arc::clone(&reads.show),
            assumes: false,
            members: Vec::new(),
        }
    }
}

/// What the clauses an ask holds before its move and checks after it
/// read: what a counterexample shows, and whether one is quantified. Read
/// once for all the asks that read the same clauses.
pub(super) struct Reads {
    show: Arc<[String]>,
    quantified: bool,
}

impl Reads {
    pub(super) fn new(
        v: &Vocab<'_>,
        before: &[(String, String)],
        goal: &[(String, String)],
    ) -> Self {
        Self {
            show: shown(v, before, goal).into(),
            quantified: before.iter().chain(goal).any(|(_, c)| quantified(c)),
        }
    }
}

fn quantified(text: &str) -> bool {
    text.contains("(forall ") || text.contains("(exists ")
}

/// The clauses a rule's proof may hold: each by name, read before and
/// after a move, and what they declare. Each solver defines them once, as
/// `pre!i` and `post!i`.
pub(super) struct Clauses {
    pub(super) decls: Vec<String>,
    pub(super) named: Vec<(String, String, String)>,
}

impl Clauses {
    /// Each clause by name, read as `frame!i`.
    pub(super) fn read(&self, frame: &str) -> Vec<(String, String)> {
        let at = |(i, (name, ..)): (usize, &(String, String, String))| {
            (name.clone(), format!("{frame}!{i}"))
        };
        self.named.iter().enumerate().map(at).collect()
    }

    /// Define each clause in `z3`. One it refuses is defined as `true`, and
    /// returned by its place with what the solver said.
    pub(super) fn define(&self, z3: &mut Z3) -> std::io::Result<Vec<(usize, String)>> {
        let decls: String = self.decls.iter().map(|d| format!("{d}\n")).collect();
        z3.send(&decls)?;
        let mut refused = Vec::new();
        for (i, (_, pre, post)) in self.named.iter().enumerate() {
            for (frame, clause) in [("pre", pre), ("post", post)] {
                let from = z3.errors.len();
                z3.send(&format!("(define-fun {frame}!{i} () Bool {clause})\n"))?;
                if z3.errors.len() > from {
                    // The first says what the solver could not read.
                    let said = z3.errors.split_off(from).into_iter().next();
                    refused.push((i, said.map(plain).unwrap_or_default()));
                    z3.send(&format!("(define-fun {frame}!{i} () Bool true)\n"))?;
                }
            }
        }
        Ok(refused)
    }
}

/// What the asks of one batch hold: every clause but those `out`, which
/// they neither assume nor must keep.
pub(super) struct Kept {
    pub(super) assume: String,
    pub(super) out: BTreeSet<String>,
}

impl Kept {
    pub(super) fn new(clauses: &Clauses, out: BTreeSet<String>) -> Arc<Self> {
        let held: Vec<String> = (clauses.named.iter().enumerate())
            .filter(|(_, (name, ..))| !out.contains(name))
            .map(|(i, _)| format!("pre!{i}"))
            .collect();
        let assume = format!("(assert (and true {}))\n", held.join(" "));
        Arc::new(Self { assume, out })
    }
}

/// A solver's error without the line of its own script it was at: every
/// solver reads the same text, and refuses it the same.
pub(super) fn plain(error: String) -> String {
    match error.split_once(": ") {
        Some((at, said)) if at.contains(" column ") => format!("(error \"{said}"),
        _ => error,
    }
}

/// The string literals `text` names, as `|'x'|`.
pub(super) fn quoted_literals(text: &str) -> Vec<String> {
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

/// Check that `goal` holds after `ask` given what it assumes and `assume`;
/// on a failure, which clauses fail and the state it shows.
pub(super) fn check(
    z3: &mut Z3,
    literals: &[String],
    ask: &Ask,
    (assume, goal): (&str, &[(String, String)]),
) -> std::io::Result<Obligation> {
    let show = [&ask.show[..], literals].concat();
    let all: Vec<&str> = goal.iter().map(|(_, c)| c.as_str()).collect();
    let negated = format!("(assert (not (and true {})))\n", all.join(" "));
    let text = format!("{}{assume}", ask.text);
    z3.send(&text)?;
    // Over all states, quantified lemmas can leave the solver searching for
    // a model until its timeout. Among states with a few dispatches of each
    // kind in flight one is quick to find: first whether the move can
    // happen, then a failure, which is a real counterexample.
    let few_first = ask.quantified;
    let (mut some, mut took) = (Answer::Unknown, Duration::ZERO);
    if few_first {
        z3.send("(push 1)\n(assert few!)\n")?;
        (some, took) = z3.check()?;
    }
    if some == Answer::Sat {
        z3.send(&negated)?;
        let (fails, also) = z3.check()?;
        took += also;
        if fails == Answer::Sat {
            let cti = z3.values(&show)?;
            z3.send("(pop 1)\n(push 1)\n(assert few!)\n")?;
            let clauses = failing(z3, goal)?;
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
    let possible = some == Answer::Sat || {
        let (any, also) = z3.check()?;
        took += also;
        any != Answer::Unsat
    };
    z3.send(&negated)?;
    let (sat, also) = z3.check()?;
    took += also;
    let outcome = match sat {
        Answer::Unsat => Outcome::Holds,
        Answer::Unknown => Outcome::Unknown,
        Answer::Sat => {
            let cti = z3.values(&show)?;
            z3.send("(pop 1)\n")?;
            z3.send(&text)?;
            let clauses = failing(z3, goal)?;
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
/// Each model that breaks one of those left shows every clause it breaks,
/// so a few checks find them all; a clause the models leave undecided is
/// checked on its own.
fn failing(z3: &mut Z3, goal: &[(String, String)]) -> std::io::Result<Vec<String>> {
    let named = |i: usize| format!("goal!{i}");
    let mut text = String::from("(push 1)\n");
    for (i, (_, c)) in goal.iter().enumerate() {
        text.push_str(&format!("(define-fun {} () Bool {c})\n", named(i)));
    }
    z3.send(&text)?;
    let mut fails = vec![false; goal.len()];
    loop {
        let left: Vec<String> = (0..goal.len()).filter(|&i| !fails[i]).map(named).collect();
        z3.send(&format!(
            "(push 1)\n(assert (not (and true {})))\n",
            left.join(" ")
        ))?;
        let (answer, _) = z3.check()?;
        let shown = match answer {
            Answer::Sat => z3.values(&left)?,
            _ => String::new(),
        };
        z3.send("(pop 1)\n")?;
        let found = (0..goal.len())
            .filter(|&i| !fails[i] && shown.contains(&format!("({} false)", named(i))))
            .collect::<Vec<_>>();
        if answer == Answer::Unsat {
            break;
        }
        if found.is_empty() {
            let undecided: Vec<usize> = (0..goal.len()).filter(|&i| !fails[i]).collect();
            for i in undecided {
                z3.send(&format!("(push 1)\n(assert (not {}))\n", named(i)))?;
                fails[i] = z3.check()?.0 != Answer::Unsat;
                z3.send("(pop 1)\n")?;
            }
            break;
        }
        found.into_iter().for_each(|i| fails[i] = true);
    }
    z3.send("(pop 1)\n")?;
    Ok(goal
        .iter()
        .zip(fails)
        .filter(|(_, fails)| *fails)
        .map(|((name, _), _)| name.clone())
        .collect())
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
