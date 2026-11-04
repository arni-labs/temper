//! An invariant's walk in steps: first with one dispatch in flight at a time,
//! then more. Most breaks need little in flight, and a walk with little is
//! far smaller, so they come out soon. What a smaller walk reaches the runtime
//! can reach, so a break it finds is broken. Only the walk with all that is
//! let in flight can say a rule holds.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::super::super::JointOutcome;
use super::super::{BrokenAt, JointModel, MAX_IN_FLIGHT, Walk};
use crate::composite::joint::room::Bound;
use temper_spec::predicate::Expr;

impl JointModel {
    /// Walk every reachable joint state in steps, checking `invariants` of
    /// `owner` in each settled one ([`Self::walk_once`]). A rule found broken
    /// is done; the others go on to the next step. It ends when a step held
    /// nothing back, since a wider one walks the same states, or runs out of room.
    pub(in crate::composite::joint) fn walk(
        self: &Arc<Self>,
        owner: &str,
        invariants: &[&Expr],
        bound: Bound<'_>,
    ) -> Walk {
        let mut found: Vec<Option<(JointOutcome, BrokenAt)>> = vec![None; invariants.len()];
        let mut open: Vec<usize> = (0..invariants.len()).collect();
        let mut in_flight = 1;
        let last = loop {
            self.in_flight.store(in_flight, Ordering::Relaxed);
            let rules: Vec<&Expr> = open.iter().map(|&rule| invariants[rule]).collect();
            let mut walk = self.walk_once(owner, &rules, bound);
            let mut outcomes = std::mem::take(&mut walk.outcomes).into_iter();
            let mut left = Vec::new();
            let mut rest = Vec::new();
            for &rule in &open {
                let outcome = outcomes.next().expect("one outcome per rule");
                if matches!(outcome, JointOutcome::Broken { .. }) {
                    let at = BrokenAt {
                        in_flight,
                        states: walk.states,
                    };
                    found[rule] = Some((outcome, at));
                } else {
                    left.push(rule);
                    rest.push(outcome);
                }
            }
            let done =
                left.is_empty() || walk.stopped || !walk.held_back || in_flight == MAX_IN_FLIGHT;
            if !done {
                open = left;
                in_flight += 1;
                continue;
            }
            walk.outcomes = rest;
            let mut outside = vec![BTreeSet::new(); invariants.len()];
            for (&rule, from) in open.iter().zip(std::mem::take(&mut walk.outside)) {
                outside[rule] = from;
            }
            walk.outside = outside;
            break walk;
        };
        self.in_flight.store(MAX_IN_FLIGHT, Ordering::Relaxed);
        let stopped = last.stopped;
        let mut outcomes = last.outcomes.into_iter();
        let mut broken = vec![None; invariants.len()];
        let all = found
            .into_iter()
            .enumerate()
            .map(|(rule, found)| match found {
                Some((outcome, at)) => {
                    broken[rule] = Some(at);
                    outcome
                }
                None => {
                    let outcome = outcomes.next().expect("one outcome per open rule");
                    if stopped {
                        finished(outcome, in_flight)
                    } else {
                        outcome
                    }
                }
            })
            .collect();
        Walk {
            states: last.states,
            outcomes: all,
            held_back: last.held_back,
            outside: last.outside,
            broken,
            stopped,
        }
    }
}

/// A rule's outcome from the walk with `in_flight` that ran out of room, saying
/// how far the walks before it got.
fn finished(outcome: JointOutcome, in_flight: usize) -> JointOutcome {
    let JointOutcome::Incomplete { reason } = outcome else {
        return outcome;
    };
    let done = in_flight - 1;
    let reason = if done == 0 {
        format!("{reason}; no walk finished, the first let {in_flight} dispatch be in flight")
    } else {
        format!("{reason}; the walk with at most {done} in flight finished without a break")
    };
    JointOutcome::Incomplete { reason }
}
