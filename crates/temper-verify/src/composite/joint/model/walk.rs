//! The breadth-first walk over joint states.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use super::super::JointState;
use super::JointModel;
use crate::Budget;

/// A move between two walked states.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Edge {
    pub(in crate::composite::joint) from: u32,
    pub(in crate::composite::joint) to: u32,
    /// The system made it: a timer, a module, or the kernel's dispatches.
    pub(in crate::composite::joint) by_system: bool,
    /// It sends an action outside code is relied on to send.
    pub(in crate::composite::joint) relied: bool,
    /// The model knows it can happen: it took no guard it could not decide.
    pub(in crate::composite::joint) certain: bool,
}

/// What a walk saw.
pub(in crate::composite::joint) struct Explored {
    /// Each state walked, with the state and move it was first reached by.
    pub(in crate::composite::joint) nodes: Vec<(JointState, Option<(usize, String)>)>,
    /// Every move between walked states, when asked for.
    pub(in crate::composite::joint) edges: Vec<Edge>,
    /// Why the walk stopped before seeing every state.
    pub(in crate::composite::joint) stopped: Option<String>,
    /// Whether a move was held back for [`super::MAX_IN_FLIGHT`].
    pub(in crate::composite::joint) held_back: bool,
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
            edges: Vec::new(),
            stopped: None,
            held_back: false,
        };
        let mut seen: HashMap<JointState, usize> = HashMap::new();
        let mut queue = VecDeque::new();
        for init in self.inits() {
            if !seen.contains_key(&init) {
                seen.insert(init.clone(), explored.nodes.len());
                queue.push_back(explored.nodes.len());
                explored.nodes.push((init, None));
            }
        }
        while let Some(index) = queue.pop_front() {
            let state = explored.nodes[index].0.clone();
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
                let relied = self.relies(&mv);
                for next in nexts {
                    let at = match seen.get(&next.state) {
                        Some(&at) => at,
                        None => {
                            let at = explored.nodes.len();
                            seen.insert(next.state.clone(), at);
                            explored.nodes.push((next.state, Some((index, next.label))));
                            queue.push_back(at);
                            at
                        }
                    };
                    if edges {
                        explored.edges.push(Edge {
                            from: index as u32,
                            to: at as u32,
                            by_system: mv.by_system(),
                            relied,
                            certain: next.certain,
                        });
                    }
                }
            }
        }
        explored
    }
}
