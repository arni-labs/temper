//! Running a rule's obligations: each is written out as text first, then
//! checked by one of a few z3 processes that each hold the vocabulary.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::encode::{Enc, prelude};
use super::solver::{Answer, Z3};
use super::vocab::Vocab;
use super::{Obligation, Outcome, Shared};

/// One obligation written out: what it assumes, what must follow, and what
/// a counterexample shows.
pub(super) struct Ask {
    pub(super) name: String,
    /// Starts with `(push 1)`: its declarations and what it assumes.
    pub(super) text: String,
    pub(super) goal: Vec<(String, String)>,
    pub(super) show: Vec<String>,
    /// A general move's ([`super::general`]): when it does not hold, the
    /// proof checks the moves it stands for, so it stops nothing.
    pub(super) general: bool,
    /// Whether a clause it reads is quantified.
    pub(super) quantified: bool,
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
        let quantified = |t: &str| t.contains("(forall ") || t.contains("(exists ");
        let quantified =
            quantified(&text) || before.iter().chain(&goal).any(|(_, c)| quantified(c));
        Self {
            name: name.to_string(),
            text,
            goal,
            show,
            general: false,
            quantified,
        }
    }
}

/// What one solver checked, by each obligation's place, and what it refused.
type Checked = (Vec<(usize, Obligation)>, Vec<String>);

/// What every solver of one rule starts from, and when they stop.
pub(super) struct Session<'s> {
    /// The vocabulary, sent to each solver first.
    pub(super) prelude: &'s str,
    /// `few!`, sent after it.
    pub(super) few: &'s str,
    /// The clauses the asks share ([`joint`]), sent last.
    pub(super) clauses: &'s str,
    /// The literals a counterexample shows.
    pub(super) literals: &'s [String],
    pub(super) solvers: usize,
    pub(super) deadline: Option<Instant>,
}

/// Check `asks` in their order over the session's solvers. A rule is not
/// proved once one obligation does not hold, so the solvers stop there,
/// unless all it breaks is `guessed` clauses, which the prover drops and
/// tries again without; the obligations checked are returned in order, with
/// what the solvers refused. They stop at the deadline too, and what they
/// did not check is returned as one obligation the solver could not decide.
pub(super) fn run(
    rule: &str,
    session: &Session<'_>,
    asks: &[Ask],
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
    let Session {
        prelude,
        few,
        clauses,
        literals,
        solvers,
        deadline,
    } = *session;
    let mut done = Vec::new();
    let mut errors = Vec::new();
    std::thread::scope(|scope| -> std::io::Result<()> {
        let workers: Vec<_> = (0..solvers)
            .map(|i| {
                scope.spawn(move || -> std::io::Result<Checked> {
                    let Some(_seat) = Seat::take(deadline) else {
                        return Ok((Vec::new(), Vec::new()));
                    };
                    let mut z3 = Z3::start()?;
                    z3.send(prelude)?;
                    z3.send(few)?;
                    z3.send(clauses)?;
                    let mut checked = Vec::new();
                    let late = || deadline.is_some_and(|d| Instant::now() >= d);
                    while !stop.load(Ordering::Relaxed) && !late() {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(ask) = asks.get(at) else {
                            break;
                        };
                        let obligation = check(&mut z3, literals, ask)?;
                        if !ask.general && ends(&obligation) {
                            stop.store(true, Ordering::Relaxed);
                        }
                        checked.push((at, obligation));
                    }
                    if let (Ok(path), Some(script)) = (std::env::var("INDUCT_SCRIPT"), &z3.script) {
                        std::fs::write(format!("{path}.{rule}.{i}.smt2"), script)?;
                    }
                    Ok((checked, std::mem::take(&mut z3.errors)))
                })
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
    let first = done
        .iter()
        .position(|(at, o)| !asks[*at].general && ends(o));
    let mut out: Vec<Obligation> = done.into_iter().map(|(_, o)| o).collect();
    if let Some(first) = first {
        out.truncate(first + 1);
    } else if out.len() < asks.len() {
        out.push(Obligation {
            name: format!(
                "{} of {} obligations, not checked by the deadline",
                asks.len() - out.len(),
                asks.len()
            ),
            took: Duration::ZERO,
            outcome: Outcome::Unknown,
            possible: true,
        });
    }
    Ok((out, errors))
}

/// The string literals `text` names, as `|'x'|`.
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

/// Check `asks` over the rule's solvers, which first read the `clauses`
/// they share, sharing the string literals they name: each distinct from
/// the others, and the empty string a parameter is checked against.
pub(super) fn solve(
    shared: &Shared<'_>,
    v: &Vocab<'_>,
    rule: &str,
    (clauses, asks): (&str, &[Ask]),
    guessed: &BTreeSet<String>,
) -> std::io::Result<(Vec<Obligation>, Vec<String>)> {
    let mut literals = BTreeSet::from([String::new()]);
    literals.extend(quoted_literals(clauses));
    for ask in asks {
        literals.extend(quoted_literals(&ask.text));
        for (_, c) in &ask.goal {
            literals.extend(quoted_literals(c));
        }
    }
    let prelude = prelude(v, &literals);
    let shown: Vec<String> = literals.iter().map(|l| Enc::lit(l)).collect();
    let few = few_in_flight(v);
    let session = Session {
        prelude: &prelude,
        few: &few,
        clauses,
        literals: &shown,
        solvers: shared.solvers.clamp(1, asks.len().max(1)),
        deadline: shared.deadline,
    };
    run(rule, &session, asks, guessed)
}

/// One of the solvers this process runs at once: as many as it has cores.
/// A solver keeps one core busy, so more only share them, and each holds
/// memory the walks' room does not count. An install proves each rule on
/// its own, all at once, so this is what bounds them.
struct Seat;

impl Seat {
    fn seats() -> &'static (Mutex<usize>, Condvar) {
        static SEATS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
        SEATS.get_or_init(|| {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            (Mutex::new(cores), Condvar::new())
        })
    }

    /// Wait for a seat, `None` once `deadline` has passed.
    fn take(deadline: Option<Instant>) -> Option<Self> {
        let (free, freed) = Self::seats();
        let mut free = free.lock().unwrap_or_else(PoisonError::into_inner);
        while *free == 0 {
            free = match deadline {
                None => freed.wait(free).unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let left = deadline.checked_duration_since(Instant::now())?;
                    freed
                        .wait_timeout(free, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        *free -= 1;
        Some(Seat)
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let (free, freed) = Self::seats();
        *free.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        freed.notify_one();
    }
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

/// J as every move's ask reads it, which each solver parses once: the
/// clauses' declarations, `j!` for the clauses before the move, and
/// `post!i` for each clause after it; with the goal that names those.
pub(super) fn joint(
    decls: &[String],
    before: &[(String, String)],
    after: &[(String, String)],
) -> (String, Vec<(String, String)>) {
    let mut text: String = decls.iter().map(|d| format!("{d}\n")).collect();
    let held: Vec<&str> = before.iter().map(|(_, c)| c.as_str()).collect();
    text.push_str(&format!(
        "(define-fun j! () Bool (and true {}))\n",
        held.join(" ")
    ));
    let mut goal = Vec::new();
    for (i, (name, c)) in after.iter().enumerate() {
        text.push_str(&format!("(define-fun post!{i} () Bool {c})\n"));
        goal.push((name.clone(), format!("post!{i}")));
    }
    (text, goal)
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
