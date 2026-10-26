//! A rule's solvers: z3 sessions that each read the vocabulary and the
//! clauses the proof may hold once, then check the asks they are given,
//! a batch at a time, until the proof ends.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use super::Obligation;
use super::Shared;
use super::encode::{Enc, prelude};
use super::run::{Ask, Clauses, Kept, check, few_in_flight, plain, quoted_literals};
use super::solver::Z3;
use super::vocab::Vocab;

/// What every solver of one rule starts from, and when they stop.
struct Session<'s> {
    /// The vocabulary, sent to each solver first.
    prelude: &'s str,
    /// `few!`, sent after it.
    few: &'s str,
    /// Defined last.
    clauses: &'s Clauses,
    /// The literals a counterexample shows.
    literals: &'s [String],
    deadline: Option<Instant>,
}

/// An ask for one solver: its place in the batch and in the proof.
struct Job {
    place: usize,
    at: usize,
    kept: Arc<Kept>,
}

/// What a solver says back.
enum Said {
    /// It started: the clauses it refused, and what else it refused.
    Ready(Vec<(usize, String)>, Vec<String>),
    /// The ask at that place in the batch, and what the solver refused.
    Checked(usize, Obligation, Vec<String>),
    /// No solver checked the ask at that place by the deadline.
    Late,
    Failed(std::io::Error),
}

/// An ask checked: its obligation, and what the solver refused there.
pub(super) type Checked = (Obligation, Vec<String>);

/// The solvers of one rule, which check its asks a batch at a time.
pub(super) struct Pool {
    jobs: Sender<Job>,
    said: Receiver<Said>,
    /// How many asks a batch holds: one for each solver.
    pub(super) width: usize,
    /// The clauses the solvers refused, by place.
    pub(super) refused: BTreeMap<usize, String>,
    /// What else they refused as they started.
    pub(super) errors: Vec<String>,
}

impl Pool {
    /// Check the asks at `at` holding `kept`; `None` for each no solver
    /// checked by the deadline.
    pub(super) fn check(
        &mut self,
        at: &[usize],
        kept: &Arc<Kept>,
    ) -> std::io::Result<Vec<Option<Checked>>> {
        let mut out: Vec<Option<Checked>> = at.iter().map(|_| None).collect();
        for (place, &at) in at.iter().enumerate() {
            let kept = Arc::clone(kept);
            // The solvers hold the other end until this pool is dropped.
            let _ = self.jobs.send(Job { place, at, kept });
        }
        let mut left = at.len();
        while left > 0 {
            // None can say more once every solver has stopped.
            let Ok(said) = self.said.recv() else { break };
            match said {
                Said::Ready(refused, errors) => {
                    for (i, error) in refused {
                        self.refused.entry(i).or_insert(error);
                    }
                    self.errors.extend(errors.into_iter().map(plain));
                }
                Said::Checked(place, obligation, errors) => {
                    out[place] = Some((obligation, errors.into_iter().map(plain).collect()));
                    left -= 1;
                }
                Said::Late => left -= 1,
                Said::Failed(error) => return Err(error),
            }
        }
        Ok(out)
    }
}

/// Run `work` with the rule's solvers, which first read the vocabulary and
/// `clauses`, then check `asks` as they are given them. They share the
/// string literals the clauses and asks name: each distinct from the
/// others, and the empty string a parameter is checked against.
pub(super) fn with_pool<R>(
    shared: &Shared<'_>,
    v: &Vocab<'_>,
    rule: &str,
    (clauses, asks): (&Clauses, &[Ask]),
    work: impl FnOnce(&mut Pool) -> std::io::Result<R>,
) -> std::io::Result<R> {
    let mut literals = BTreeSet::from([String::new()]);
    for text in clauses
        .decls
        .iter()
        .chain(clauses.named.iter().flat_map(|(_, pre, post)| [pre, post]))
    {
        literals.extend(quoted_literals(text));
    }
    for ask in asks {
        literals.extend(quoted_literals(&ask.text));
        for (_, c) in ask.goal.iter() {
            literals.extend(quoted_literals(c));
        }
    }
    let prelude = prelude(v, &literals);
    let shown: Vec<String> = literals.iter().map(|l| Enc::lit(l)).collect();
    let few = few_in_flight(v);
    let session = &Session {
        prelude: &prelude,
        few: &few,
        clauses,
        literals: &shown,
        deadline: shared.deadline,
    };
    let solvers = shared.solvers.clamp(1, asks.len().max(1));
    let (jobs, taken) = channel();
    let taken = &Mutex::new(taken);
    let (says, said) = channel();
    std::thread::scope(|scope| {
        for i in 0..solvers {
            let says = says.clone();
            scope.spawn(move || {
                let name = format!("{rule}.{i}");
                if let Err(error) = serve(session, asks, taken, &says, &name) {
                    let _ = says.send(Said::Failed(error));
                }
            });
        }
        drop(says);
        let mut pool = Pool {
            jobs,
            said,
            width: solvers,
            refused: BTreeMap::new(),
            errors: Vec::new(),
        };
        let done = work(&mut pool);
        // With no more jobs to come, the solvers stop.
        drop(pool);
        done
    })
}

/// One solver: read the session, then check each ask it is given, until
/// there are no more or the deadline passes.
fn serve(
    session: &Session<'_>,
    asks: &[Ask],
    taken: &Mutex<Receiver<Job>>,
    says: &Sender<Said>,
    name: &str,
) -> std::io::Result<()> {
    let Some(_seat) = Seat::take(session.deadline) else {
        return Ok(());
    };
    let mut z3 = Z3::start()?;
    z3.send(session.prelude)?;
    z3.send(session.few)?;
    let refused = session.clauses.define(&mut z3)?;
    let _ = says.send(Said::Ready(refused, std::mem::take(&mut z3.errors)));
    let late = || session.deadline.is_some_and(|d| Instant::now() >= d);
    loop {
        let job = taken.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(Job { place, at, kept }) = job else {
            break;
        };
        if late() {
            let _ = says.send(Said::Late);
            break;
        }
        let ask = &asks[at];
        let goal: Vec<(String, String)> = (ask.goal.iter())
            .filter(|(name, _)| !kept.out.contains(name))
            .cloned()
            .collect();
        let assume = if ask.assumes {
            kept.assume.as_str()
        } else {
            ""
        };
        let obligation = check(&mut z3, session.literals, ask, (assume, &goal))?;
        let errors = std::mem::take(&mut z3.errors);
        let _ = says.send(Said::Checked(place, obligation, errors));
    }
    if let (Ok(path), Some(script)) = (std::env::var("INDUCT_SCRIPT"), &z3.script) {
        std::fs::write(format!("{path}.{name}.smt2"), script)?;
    }
    Ok(())
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
