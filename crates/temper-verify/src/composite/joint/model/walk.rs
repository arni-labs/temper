//! The breadth-first walk over joint states.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use super::super::pack::{Packed, Packer};
use super::super::state::render;
use super::super::{JointState, JointStep};
use super::JointModel;
use super::step::Certainty;
use crate::Budget;

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

/// What a walk saw.
pub(in crate::composite::joint) struct Explored {
    /// Each state walked, packed, with the state and move (a name in
    /// `packer`) it was first reached by.
    nodes: Vec<(Packed, Option<(u32, u32)>)>,
    packer: Packer,
    /// Every move made from a walked state, when asked for.
    pub(in crate::composite::joint) made: Vec<Made>,
    /// Every way those moves lead between walked states.
    pub(in crate::composite::joint) edges: Vec<Edge>,
    /// Why the walk stopped before seeing every state.
    pub(in crate::composite::joint) stopped: Option<String>,
    /// Whether a move was held back for [`super::MAX_IN_FLIGHT`].
    pub(in crate::composite::joint) held_back: bool,
    /// The moves the runtime could make in a walked state that the walk
    /// never takes.
    unwalked: BTreeSet<Unwalked>,
}

/// A move the runtime could make that the walk never takes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) struct Unwalked {
    /// `Entity.Action`.
    pub(in crate::composite::joint) action: String,
    /// Who sends it in the runtime.
    pub(in crate::composite::joint) sender: String,
    /// The record's status where it could.
    pub(in crate::composite::joint) from: String,
}

impl Explored {
    /// How many states the walk saw.
    pub(in crate::composite::joint) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The state walked `index`th.
    pub(in crate::composite::joint) fn state(&self, index: usize) -> JointState {
        self.packer.unpack(&self.nodes[index].0)
    }

    /// The moves from the start to the state walked `index`th.
    pub(in crate::composite::joint) fn path(&self, mut index: usize) -> Vec<JointStep> {
        let mut steps = Vec::new();
        loop {
            let parent = self.nodes[index].1;
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

    /// The moves the runtime could make that the walk never took, each as
    /// `Entity.Action (from Status, ...), sent by who`.
    pub(in crate::composite::joint) fn unwalked(&self) -> Vec<String> {
        let mut from: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
        for found in &self.unwalked {
            from.entry((&found.action, &found.sender))
                .or_default()
                .push(&found.from);
        }
        from.into_iter()
            .map(|((action, sender), statuses)| {
                format!("{action} (from {}), sent by {sender}", statuses.join(", "))
            })
            .collect()
    }
}

/// Why a walk that never took the moves `unwalked` proves nothing.
pub(in crate::composite::joint) fn unwalked_reason(unwalked: &[String]) -> String {
    format!(
        "the walk never takes {}, so it never goes where they lead. Declare an action only a module answers with as internal, or name it in its trigger's on_success; the walk does not send a reaction from a type outside the rule's scope",
        unwalked.join("; ")
    )
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
    /// every move between walked states is kept.
    pub(in crate::composite::joint) fn walk_with(
        &self,
        budget: Budget,
        edges: bool,
        mut visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored {
        let started = Instant::now();
        let mut explored = Explored {
            nodes: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
            unwalked: BTreeSet::new(),
        };
        let mut seen: HashMap<Packed, u32> = HashMap::new();
        let mut queue = VecDeque::new();
        for init in self.inits() {
            let packed = explored.packer.pack(&init);
            if !seen.contains_key(&packed) {
                seen.insert(packed.clone(), explored.nodes.len() as u32);
                queue.push_back(explored.nodes.len());
                explored.nodes.push((packed, None));
            }
        }
        while let Some(index) = queue.pop_front() {
            let state = explored.state(index);
            self.note_unwalked(&state, &mut explored.unwalked);
            if visit(index, &state) {
                break;
            }
            let full = explored.nodes.len() >= budget.states;
            if full || started.elapsed() >= budget.time {
                explored.stopped = Some(if full {
                    format!("the walk reached its {} states", budget.states)
                } else {
                    format!("the walk ran out of its {} s", budget.time.as_secs())
                });
                break;
            }
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
                    let packed = explored.packer.pack(&next.state);
                    let at = match seen.get(&packed) {
                        Some(&at) => at,
                        None => {
                            let at = explored.nodes.len() as u32;
                            let label = explored.packer.id(&next.label);
                            seen.insert(packed.clone(), at);
                            explored.nodes.push((packed, Some((index as u32, label))));
                            queue.push_back(at as usize);
                            at
                        }
                    };
                    if edges {
                        explored.edges.push(Edge {
                            made,
                            to: at,
                            sure: next.certainty == Certainty::Sure,
                        });
                    }
                }
            }
        }
        explored
    }
}
