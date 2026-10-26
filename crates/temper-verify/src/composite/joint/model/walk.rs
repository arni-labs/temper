//! The breadth-first walk over joint states.

use super::super::pack::Packer;
use super::super::room::{Bound, Held, MB, bytes, grow};
use super::super::state::render;
use super::super::store::States;
use super::super::{JointState, JointStep};
use super::JointModel;
use super::step::Certainty;

/// One move made from a walked state.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Made {
    pub(in crate::composite::joint) from: u32,
    /// The system made it: a timer, a module, or the kernel's dispatches.
    pub(in crate::composite::joint) by_system: bool,
    /// It sends an action outside code is relied on to send.
    pub(in crate::composite::joint) relied: bool,
    /// It always ends in one of the states its edges reach: it leads
    /// somewhere, none of its states is unsure, and none was held back.
    pub(in crate::composite::joint) covered: bool,
}

/// A move's way from one walked state to another.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Edge {
    /// The move, an index into [`Explored::made`].
    pub(in crate::composite::joint) made: u32,
    pub(in crate::composite::joint) to: u32,
    /// The move can lead here.
    pub(in crate::composite::joint) sure: bool,
}

/// What a walk saw, and the memory it holds for it.
pub(in crate::composite::joint) struct Explored<'a> {
    /// Each state walked, packed.
    states: States,
    /// The state and move (a name in `packer`) each was first reached by.
    firsts: Vec<Option<(u32, u32)>>,
    packer: Packer,
    /// Every move made from a walked state, when asked for.
    pub(in crate::composite::joint) made: Vec<Made>,
    /// Every way those moves lead between walked states.
    pub(in crate::composite::joint) edges: Vec<Edge>,
    /// Why the walk stopped before seeing every state.
    pub(in crate::composite::joint) stopped: Option<String>,
    /// Whether a move was held back for [`super::MAX_IN_FLIGHT`].
    pub(in crate::composite::joint) held_back: bool,
    /// The memory its tables hold, given back when it is dropped.
    pub(in crate::composite::joint) held: Held<'a>,
}

impl Explored<'_> {
    fn stop_for_room(&mut self, reason: String) {
        self.stopped = Some(format!(
            "the walk does not fit in the memory it may hold: {reason} \
             (it held {} MB at {} states)",
            self.held.bytes / MB,
            self.len()
        ));
    }

    /// How many states the walk saw.
    pub(in crate::composite::joint) fn len(&self) -> usize {
        self.states.len()
    }

    /// The state walked `index`th.
    pub(in crate::composite::joint) fn state(&self, index: usize) -> JointState {
        self.packer.unpack(self.states.get(index))
    }

    /// The moves from the start to the state walked `index`th.
    pub(in crate::composite::joint) fn path(&self, mut index: usize) -> Vec<JointStep> {
        let mut steps = Vec::new();
        loop {
            let parent = self.firsts[index];
            steps.push(JointStep {
                action: parent.map(|(_, label)| self.packer.name(label).to_string()),
                state: render(&self.state(index)),
            });
            match parent {
                Some((from, _)) => index = from as usize,
                None => break,
            }
        }
        steps.reverse();
        steps
    }
}

impl JointModel {
    /// Whether `mv` sends an action outside code is relied on to send.
    pub(in crate::composite::joint) fn relies(&self, mv: &super::Move) -> bool {
        mv.outside_action().is_some_and(|(entity, action)| {
            self.relies_on
                .contains(&(entity.to_string(), action.to_string()))
        })
    }

    /// Walk every reachable joint state breadth first, so each state is
    /// first reached by a shortest path. `visit` sees each state as it is
    /// taken from the queue and returns true to stop the walk. With `edges`,
    /// every move between walked states is kept. The walk takes the memory
    /// it holds from `bound`'s room and stops when the room says no.
    pub(in crate::composite::joint) fn walk_with<'a>(
        &self,
        bound: Bound<'a>,
        edges: bool,
        mut visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored<'a> {
        let mut explored = Explored {
            states: States::default(),
            firsts: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
            held: Held::new(bound),
        };
        let mut tables = Tables::default();
        let mut packed = Vec::new();
        for init in self.inits() {
            explored.packer.pack(&init, &mut packed);
            let new = match explored.states.add(&packed, &mut explored.held) {
                Ok((_, new)) => new,
                Err(reason) => {
                    explored.stop_for_room(reason);
                    return explored;
                }
            };
            if new {
                explored.firsts.push(None);
            }
        }
        // States are walked in the order they were first reached, which is
        // breadth first.
        let mut index = 0;
        'walk: while index < explored.len() {
            let state = explored.state(index);
            if visit(index, &state) {
                break;
            }
            explored.held.walked(explored.len());
            if let Err(reason) = tables.make_room(&mut explored, edges) {
                explored.stop_for_room(reason);
                break;
            }
            let before = [explored.len(), explored.made.len(), explored.edges.len()];
            for mv in self.moves(&state) {
                let (nexts, held) = self.next(&state, &mv);
                explored.held_back |= held;
                if edges {
                    explored.made.push(Made {
                        from: index as u32,
                        by_system: mv.by_system(),
                        relied: self.relies(&mv),
                        covered: !held
                            && !nexts.is_empty()
                            && nexts.iter().all(|n| n.certainty != Certainty::Unsure),
                    });
                }
                let made = explored.made.len().saturating_sub(1) as u32;
                for next in nexts {
                    explored.packer.pack(&next.state, &mut packed);
                    let (at, new) = match explored.states.add(&packed, &mut explored.held) {
                        Ok(added) => added,
                        Err(reason) => {
                            explored.stop_for_room(reason);
                            break 'walk;
                        }
                    };
                    if new {
                        let label = explored.packer.id(&next.label);
                        explored.firsts.push(Some((index as u32, label)));
                    }
                    if edges {
                        explored.edges.push(Edge {
                            made,
                            to: at,
                            sure: next.certainty == Certainty::Sure,
                        });
                    }
                }
            }
            let after = [explored.len(), explored.made.len(), explored.edges.len()];
            tables.saw([0, 1, 2].map(|i| after[i] - before[i]));
            index += 1;
        }
        explored
    }
}

/// The walk's tables as the memory it holds counts them: each at its
/// capacity. Before each state is walked, every table is grown to hold what
/// walking one state has added at most so far, with the memory of its new
/// copy taken first, so a table seldom grows on its own while both copies
/// are held.
#[derive(Default)]
struct Tables {
    /// The most states, moves and edges walking one state has added.
    most: [usize; 3],
}

impl Tables {
    fn saw(&mut self, added: [usize; 3]) {
        for (most, added) in self.most.iter_mut().zip(added) {
            *most = (*most).max(added);
        }
    }

    fn make_room(&mut self, explored: &mut Explored<'_>, edges: bool) -> Result<(), String> {
        let [states, made, ends] = self.most.map(|most| most.max(1));
        let held = &mut explored.held;
        explored.states.reserve(states, held)?;
        grow(&mut explored.firsts, states, held)?;
        if edges {
            grow(&mut explored.made, made, held)?;
            grow(&mut explored.edges, ends, held)?;
        }
        let total = explored.states.bytes()
            + explored.packer.bytes()
            + bytes(&explored.firsts)
            + bytes(&explored.made)
            + bytes(&explored.edges);
        explored.held.set(total)
    }
}
