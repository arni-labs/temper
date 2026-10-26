//! Dropping the clauses a move breaks, until the rest hold (Houdini).
//!
//! The prover's guesses and the lemmas an agent writes only help a proof:
//! each one a move breaks is dropped, and the proof goes on without it.
//! Dropping a clause only weakens what a move assumes, so a clause a move
//! breaks while more are held it breaks with fewer too. The clauses left at
//! the end are the same whatever order the moves are checked in.
//!
//! The asks are checked a batch at a time, one for each solver, holding the
//! clauses held when the batch starts; what a batch breaks is dropped
//! before the next. A pass checks, in order, each ask not yet seen to hold
//! with the clauses held now. The proof ends after a pass that drops
//! nothing, or at a move that breaks what it may not drop.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use super::pool::Pool;
use super::run::{Ask, Clauses, Goal, Kept};
use super::{Obligation, Outcome};

/// Where a proof's clauses ended.
pub(super) struct Settled {
    /// Each ask's obligation, in order. All hold, but the last when a move
    /// broke what it may not drop or the deadline passed.
    pub(super) obligations: Vec<Obligation>,
    /// What the solvers refused.
    pub(super) errors: Vec<String>,
    /// The clauses dropped on the way, each with why.
    pub(super) dropped: BTreeMap<usize, String>,
    /// Every clause not held at the end: dropped, or left out from the start.
    pub(super) out: BTreeSet<usize>,
}

impl Settled {
    pub(super) fn proved(&self) -> bool {
        self.errors.is_empty()
            && (self.obligations.iter()).all(|o| matches!(o.outcome, Outcome::Holds))
    }
}

/// How a proof ends short of holding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The ask at that place broke what it may not drop.
    At(usize),
    Late,
}

/// Check `asks`, which the solvers of `pool` hold at the same places,
/// dropping each clause of `droppable` a move breaks or the solver refuses.
/// The clauses `left_out` are not held from the start. `None` once every
/// clause it may drop is gone, unless the proof may go on with none.
pub(super) fn settle(
    pool: &mut Pool,
    (clauses, asks): (&Clauses, &[Ask]),
    droppable: &BTreeSet<usize>,
    (left_out, may_end_bare): (&BTreeSet<usize>, bool),
) -> std::io::Result<Option<Settled>> {
    let n = asks.len();
    // A general move's members stand in for it while a clause it broke is
    // held: a move that breaks a clause breaks it with fewer held, but one
    // dropped no longer counts.
    let mut stands = vec![true; n];
    for &m in asks.iter().flat_map(|a| &a.members) {
        stands[m] = false;
    }
    let mut broke: Vec<Option<Vec<Goal>>> = vec![None; n];
    // How many drops there had been when each ask last held.
    let mut held: Vec<Option<usize>> = vec![None; n];
    let mut last: Vec<Option<Obligation>> = vec![None; n];
    let (mut drops, mut dropped, mut errors) = (0, BTreeMap::new(), Vec::new());
    let stop = 'proof: loop {
        let mut due: VecDeque<usize> = (0..n)
            .filter(|&i| stands[i] && held[i] != Some(drops))
            .collect();
        if due.is_empty() {
            break None;
        }
        while !due.is_empty() {
            due.retain(|&i| stands[i]);
            let batch: Vec<usize> = due.drain(..due.len().min(pool.width)).collect();
            let out = left_out.iter().chain(dropped.keys()).cloned().collect();
            let checked = pool.check(&batch, &Kept::new(clauses, out))?;
            // What the solvers refused as they started comes first, in every
            // try: a clause they refuse may only name what that left
            // undeclared.
            note(&mut errors, pool.errors.iter().cloned());
            for (&i, error) in &pool.refused {
                if droppable.contains(&i) {
                    let why = || format!("the solver refuses: {error}");
                    dropped.entry(i).or_insert_with(why);
                } else {
                    note(&mut errors, [error.clone()]);
                }
            }
            let mut broken = BTreeMap::new();
            let mut instead = Vec::new();
            for (&i, checked) in batch.iter().zip(checked) {
                let Some((obligation, failed, refused)) = checked else {
                    break 'proof Some(Stop::Late);
                };
                note(&mut errors, refused);
                match &obligation.outcome {
                    Outcome::Holds => held[i] = Some(drops),
                    outcome if !asks[i].members.is_empty() => {
                        stands[i] = false;
                        broke[i] = match outcome {
                            Outcome::Fails { .. } if !failed.is_empty() => Some(failed.clone()),
                            _ => None,
                        };
                        for &m in &asks[i].members {
                            stands[m] = true;
                            instead.push(m);
                        }
                    }
                    Outcome::Fails { .. }
                        if !failed.is_empty()
                            && failed
                                .iter()
                                .all(|g| matches!(g, Goal::Clause(i) if droppable.contains(i))) =>
                    {
                        held[i] = None;
                        for goal in failed {
                            if let Goal::Clause(c) = goal {
                                let why = || format!("fails after {}", obligation.name);
                                broken.entry(c).or_insert_with(why);
                            }
                        }
                    }
                    _ => {
                        last[i] = Some(obligation);
                        break 'proof Some(Stop::At(i));
                    }
                }
                last[i] = Some(obligation);
            }
            if !errors.is_empty() {
                break 'proof None;
            }
            // The moves a general move stands for are checked next.
            for m in instead.into_iter().rev() {
                due.push_front(m);
            }
            if !broken.is_empty() {
                dropped.extend(broken);
                drops += 1;
                for i in 0..n {
                    let gone = |goals: &Vec<Goal>| {
                        goals
                            .iter()
                            .all(|g| matches!(g, Goal::Clause(i) if dropped.contains_key(i)))
                    };
                    if broke[i].as_ref().is_some_and(gone) {
                        broke[i] = None;
                        stands[i] = true;
                        asks[i].members.iter().for_each(|&m| stands[m] = false);
                    }
                }
                let left = droppable
                    .iter()
                    .any(|c| !left_out.contains(c) && !dropped.contains_key(c));
                if !left && !may_end_bare {
                    return Ok(None);
                }
            }
        }
    };
    let mut obligations: Vec<Obligation> = (0..n)
        .filter(|&i| stands[i] && (held[i].is_some() || stop == Some(Stop::At(i))))
        .filter_map(|i| last[i].clone())
        .collect();
    if stop == Some(Stop::Late) {
        let standing = (0..n).filter(|&i| stands[i]);
        let unchecked = standing.clone().filter(|&i| held[i] != Some(drops));
        obligations.push(Obligation {
            name: format!(
                "{} of {} obligations, not checked by the deadline",
                unchecked.count(),
                standing.count()
            ),
            took: Duration::ZERO,
            outcome: Outcome::Unknown,
            possible: true,
        });
    }
    let out = left_out.iter().chain(dropped.keys()).cloned().collect();
    Ok(Some(Settled {
        obligations,
        errors,
        dropped,
        out,
    }))
}

/// Add each of `said` to `errors` it does not hold yet.
fn note(errors: &mut Vec<String>, said: impl IntoIterator<Item = String>) {
    for error in said {
        if !errors.contains(&error) {
            errors.push(error);
        }
    }
}
