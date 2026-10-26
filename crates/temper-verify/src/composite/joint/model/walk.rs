//! The breadth-first walk over joint states.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use super::super::JointOutcome;
use super::super::pack::{Packed, Packer};
use super::super::state::render;
use super::super::{JointState, JointStep};
use super::step::Certainty;
use super::{JointModel, Walk};
use crate::Budget;
use temper_spec::predicate::{Expr, Name, Truth, eval};

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
        self.expansion.reset(budget.states, Some(budget.time));
        let mut explored = Explored {
            nodes: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
        };
        // Check the sliced models before either caller can expand a move.
        let oversized: Vec<_> = self
            .models
            .iter()
            .filter_map(|(entity, model)| {
                crate::model::semantics::oversized_transition(model)
                    .map(|(action, _)| format!("{entity}.{action} parameter expansion"))
            })
            .collect();
        if !oversized.is_empty() {
            explored.stopped = Some(format!(
                "unsupported reads or expansion: {}",
                oversized.join(", ")
            ));
            return explored;
        }
        let mut seen: HashMap<Packed, u32> = HashMap::new();
        let mut queue = VecDeque::new();
        for init in self.inits() {
            if !self.expansion.take() {
                break;
            }
            let packed = explored.packer.pack(&init);
            if !seen.contains_key(&packed) {
                seen.insert(packed.clone(), explored.nodes.len() as u32);
                queue.push_back(explored.nodes.len());
                explored.nodes.push((packed, None));
            }
        }
        while let Some(index) = queue.pop_front() {
            let state = explored.state(index);
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
                if !self.expansion.take() {
                    break;
                }
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
        if self.expansion.exhausted.get() {
            explored.stopped = Some("the joint expansion exhausted its work or time budget".into());
        }
        explored
    }
}

impl JointModel {
    /// Walk every reachable joint state breadth first, checking `invariants`
    /// of `owner` in each settled one (nothing in flight).
    pub(in crate::composite::joint) fn walk(
        &self,
        owner: &str,
        invariants: &[&Expr],
        budget: Budget,
    ) -> Walk {
        let mut unsupported = BTreeSet::new();
        let modeled = |entity: &str, field: &str| {
            self.models.get(entity).is_some_and(|model| {
                matches!(field, "Id" | "id")
                    || match model.var_kinds.get(field) {
                        Some(temper_spec::predicate::VarKind::Str) => {
                            model.tracked_strings.contains(field)
                        }
                        Some(
                            temper_spec::predicate::VarKind::Counter
                            | temper_spec::predicate::VarKind::Bool
                            | temper_spec::predicate::VarKind::List,
                        ) => true,
                        _ => false,
                    }
            })
        };
        for inv in invariants {
            inv.for_each_name(&mut |name| {
                let mut check = |entity: &str, field: &str| {
                    if !modeled(entity, field) {
                        unsupported.insert(format!("{entity}.{field}"));
                    }
                };
                match name {
                    Name::Var(field) => check(owner, field),
                    Name::CrossStatus { id_field, .. } => check(owner, id_field),
                    Name::CrossField {
                        entity_type,
                        id_field,
                        field,
                    } => {
                        check(owner, id_field);
                        check(entity_type, field);
                    }
                    Name::Status | Name::Param(_) => {}
                }
            });
        }
        if !unsupported.is_empty() {
            let reason = format!(
                "unsupported reads or expansion: {}",
                unsupported.into_iter().collect::<Vec<_>>().join(", ")
            );
            return Walk {
                states: 0,
                held_back: false,
                outcomes: invariants
                    .iter()
                    .map(|_| JointOutcome::Incomplete {
                        reason: reason.clone(),
                    })
                    .collect(),
            };
        }
        let mut unknown = vec![false; invariants.len()];
        let mut broken: Vec<Option<usize>> = vec![None; invariants.len()];
        let explored = self.walk_with(budget, false, |index, state| {
            if state.settled() {
                let env = self.env(owner, state);
                for (i, inv) in invariants.iter().enumerate() {
                    match eval(inv, &env) {
                        Truth::False if broken[i].is_none() => broken[i] = Some(index),
                        Truth::Unknown => unknown[i] = true,
                        _ => {}
                    }
                }
            }
            broken.iter().all(Option::is_some)
        });
        let outcomes = broken
            .iter()
            .enumerate()
            .map(|(i, found)| match found {
                Some(index) => JointOutcome::Broken {
                    path: explored.path(*index),
                },
                None if unknown[i] => JointOutcome::Incomplete {
                    reason: "an invariant read is unknown".into(),
                },
                None => match &explored.stopped {
                    Some(reason) => JointOutcome::Incomplete {
                        reason: reason.clone(),
                    },
                    None => JointOutcome::Holds,
                },
            })
            .collect();
        Walk {
            states: explored.len(),
            outcomes,
            held_back: explored.held_back,
        }
    }
}
