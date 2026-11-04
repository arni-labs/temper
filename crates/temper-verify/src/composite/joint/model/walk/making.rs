//! Making a walked state's moves, each from what the thread remembers of it
//! or afresh, and looking up where they lead.

use std::sync::atomic::Ordering;

use super::super::super::JointState;
use super::super::super::state::render;
use super::super::step::{Certainty, Move};
use super::checked::{BATCH, To, Walked, Walker, Walking, Way};
use super::levels::deeper;
use super::memo::{Group, check_step};
use super::shared::{read, write};
use super::{Keep, Made, Via};

impl Walker {
    /// Make `group`'s moves from the state. None when the walk ended first.
    pub(super) fn walk_moves(
        &self,
        group: &mut Group,
        walking: &mut Walking<'_>,
        jobs: &mut Vec<Walked>,
    ) -> Option<()> {
        for at in 0..group.moves.len() {
            let by = walking.by;
            walking.by += 1;
            if group.remembers(at) {
                self.walk_remembered(group, at, by, walking)?;
            } else {
                self.walk_afresh(group, at, by, walking)?;
            }
            if walking.batch.ways.len() >= BATCH {
                self.add(walking.job, &mut walking.batch, jobs);
            }
        }
        Some(())
    }

    /// Make move `at` of `group` from what the thread remembers of it. None
    /// when the walk ended first.
    fn walk_remembered(
        &self,
        group: &Group,
        at: usize,
        by: u32,
        walking: &mut Walking<'_>,
    ) -> Option<()> {
        let model = &*self.model;
        let state = walking.state;
        let mv = &group.moves[at];
        let unsure = model.unsure(state, mv);
        *walking.steps += 1;
        let check = check_step(*walking.steps);
        let mut made_here = Vec::new();
        let made = self.note_move(mv, walking);
        let (mut held, mut covered, mut ways) = (false, true, 0);
        for way in 0..group.way_count(at) {
            let known = group.way(at, way, state, walking.scratch);
            let known = if unsure { Certainty::Unsure } else { known };
            if !model.settled_quietly(walking.scratch) {
                held = true;
                continue;
            }
            if check {
                made_here.push((walking.scratch.clone(), known));
            }
            covered &= known != Certainty::Unsure;
            self.reached(known, by, ways, made, walking);
            ways += 1;
        }
        self.settle_move(made, held, covered && ways > 0, walking);
        if check {
            let (nexts, fresh_held) = self.built(|| model.next(state, mv))?;
            assert!(
                held == fresh_held
                    && made_here.len() == nexts.len()
                    && made_here
                        .iter()
                        .zip(&nexts)
                        .all(|((s, k), next)| *s == next.state && *k == next.certainty),
                "a remembered move differs from the move made afresh: {mv:?} from {}",
                render(state)
            );
        }
        Some(())
    }

    /// Make move `at` of `group` now, and remember it unless it read what is
    /// in flight. None when the walk ended first.
    fn walk_afresh(
        &self,
        group: &mut Group,
        at: usize,
        by: u32,
        walking: &mut Walking<'_>,
    ) -> Option<()> {
        let model = &*self.model;
        let state = walking.state;
        model.expansion.flight_was_read();
        let outcomes = self.built(|| model.outcomes(state, &group.moves[at]))?;
        let (nexts, held) = match outcomes {
            Some(outcomes) => {
                if !model.expansion.flight_was_read() {
                    let packer = read(&self.shared.packer);
                    let bytes = group.keep(at, state, &outcomes, &packer);
                    drop(packer);
                    self.remember(bytes);
                }
                let mv = &group.moves[at];
                self.built(|| model.finished(state, mv, outcomes.clone()))?
            }
            None => (Vec::new(), false),
        };
        let made = self.note_move(&group.moves[at], walking);
        let covered = !nexts.is_empty() && nexts.iter().all(|n| n.certainty != Certainty::Unsure);
        for (outcome, next) in nexts.iter().enumerate() {
            walking.scratch.clone_from(&next.state);
            self.reached(next.certainty, by, outcome as u32, made, walking);
        }
        self.settle_move(made, held, covered, walking);
        Some(())
    }

    /// Note a move a state makes, for a walk that keeps its moves, and say
    /// which it is.
    fn note_move(&self, mv: &Move, walking: &mut Walking<'_>) -> u32 {
        let batch = &mut walking.batch;
        if self.keep == Keep::Edges {
            batch.made.push(Made {
                from: walking.job.at,
                by_system: mv.by_system(),
                relied: self.model.relies(mv),
                outside: mv.is_outside_reaction(),
                covered: false,
                held: false,
            });
        }
        batch.made.len().saturating_sub(1) as u32
    }

    /// Say what the move `made` came to.
    fn settle_move(&self, made: u32, held: bool, covered: bool, walking: &mut Walking<'_>) {
        if held {
            self.shared.held_back.store(true, Ordering::Relaxed);
        }
        if self.keep == Keep::Edges {
            let made = &mut walking.batch.made[made as usize];
            made.held = held;
            made.covered = !held && covered;
        }
    }

    /// A move led to the state in `walking.scratch`, by way `outcome` of it.
    fn reached(
        &self,
        known: Certainty,
        by: u32,
        outcome: u32,
        made: u32,
        walking: &mut Walking<'_>,
    ) {
        let shared = &*self.shared;
        let edges = self.keep == Keep::Edges;
        self.pack(walking.scratch, walking.packed);
        let depth = deeper(walking.job.depth);
        let surely = walking.sure && known == Certainty::Sure;
        let found = shared.seen.get(walking.packed.as_slice()).map(|seen| *seen);
        if let Some(seen) = found {
            shared.levels.found_again(seen.depth, depth);
        }
        let to = match found {
            Some(seen) if self.keep != Keep::Sure || !surely || seen.sure => {
                if !edges {
                    return;
                }
                To::Known(seen.at)
            }
            _ => To::Check {
                packed: walking.packed.as_slice().into(),
                via: Via { by, outcome },
                surely,
            },
        };
        walking.batch.ways.push(Way {
            made,
            to,
            sure: known == Certainty::Sure,
        });
    }

    /// `state` packed into `out`, adding what the tables do not hold yet.
    fn pack(&self, state: &JointState, out: &mut Vec<u32>) {
        if !read(&self.shared.packer).known_into(state, out) {
            write(&self.shared.packer).pack_into(state, out);
        }
    }
}
