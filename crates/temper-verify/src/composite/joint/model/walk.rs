//! The walk over joint states: breadth first, or toward a status.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use super::super::JointOutcome;
use super::super::pack::Packer;
use super::super::room::{Bound, Held, MB, bytes, grow};
use super::super::state::render;
use super::super::store::States;
use super::super::{JointState, JointStep, premise};
use super::step::Certainty;
use super::{JointModel, Walk};
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
        visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored<'a> {
        self.walk_in(bound, edges, InOrder::default(), |_| 0, visit)
    }

    /// Walk every reachable joint state as [`Self::walk_with`] does, with no
    /// edges kept, taking next a state whose `owner` record is nearest a
    /// status `target` picks, the latest found among the nearest. A search
    /// for a state that may stand there finds one sooner, by a path that may
    /// not be the shortest. When `target` picks every status, it walks
    /// breadth first.
    pub(in crate::composite::joint) fn walk_toward<'a>(
        &self,
        bound: Bound<'a>,
        (owner, target): (&str, impl Fn(&str) -> bool),
        visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored<'a> {
        let model = self.models.get(owner);
        let Some(model) = model.filter(|m| !m.states.iter().all(|s| target(s))) else {
            return self.walk_with(bound, false, visit);
        };
        let near = model.distances(target);
        let distance = |state: &JointState| {
            let status = state.records.get(owner).map(|r| r.status.as_str());
            status
                .and_then(|s| near.get(s))
                .copied()
                .unwrap_or(usize::MAX)
        };
        self.walk_in(bound, false, BinaryHeap::new(), distance, visit)
    }

    fn walk_in<'a>(
        &self,
        bound: Bound<'a>,
        edges: bool,
        mut queue: impl Frontier,
        distance: impl Fn(&JointState) -> usize,
        mut visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored<'a> {
        // The room bounds what the walk holds; a model's expansion work has
        // no count or clock of its own here.
        self.expansion.reset(usize::MAX, None);
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
        let mut tables = Tables::default();
        let mut packed = Vec::new();
        for init in self.inits() {
            if !self.expansion.take() {
                break;
            }
            explored.packer.pack(&init, &mut packed);
            let (at, new) = match explored.states.add(&packed, &mut explored.held) {
                Ok(added) => added,
                Err(reason) => {
                    explored.stop_for_room(reason);
                    return explored;
                }
            };
            if new {
                explored.firsts.push(None);
                queue.add(at as usize, distance(&init));
            }
        }
        'walk: while let Some(index) = queue.take(explored.len()) {
            let state = explored.state(index);
            if visit(index, &state) {
                break;
            }
            explored.held.walked(explored.len());
            if let Err(reason) = tables.make_room(&mut explored, &mut queue, edges) {
                explored.stop_for_room(reason);
                break;
            }
            let before = [explored.len(), explored.made.len(), explored.edges.len()];
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
                        queue.add(at as usize, distance(&next.state));
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
        bound: Bound<'_>,
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
        let premises: Vec<Option<&Expr>> = invariants.iter().map(|inv| premise(inv)).collect();
        let mut met: Vec<bool> = premises.iter().map(Option::is_none).collect();
        let explored = self.walk_with(bound, false, |index, state| {
            if state.settled() {
                let env = self.env(owner, state);
                for (i, inv) in invariants.iter().enumerate() {
                    match eval(inv, &env) {
                        Truth::False if broken[i].is_none() => broken[i] = Some(index),
                        Truth::Unknown => unknown[i] = true,
                        _ => {}
                    }
                    if let (false, Some(premise)) = (met[i], premises[i]) {
                        met[i] = eval(premise, &env) != Truth::False;
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
                    None => match premises[i] {
                        Some(premise) if !met[i] => JointOutcome::Vacuous {
                            premise: premise.to_string(),
                        },
                        _ => JointOutcome::Holds,
                    },
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

    fn make_room(
        &mut self,
        explored: &mut Explored<'_>,
        queue: &mut impl Frontier,
        edges: bool,
    ) -> Result<(), String> {
        let [states, made, ends] = self.most.map(|most| most.max(1));
        let held = &mut explored.held;
        explored.states.reserve(states, held)?;
        queue.grow(states, held)?;
        grow(&mut explored.firsts, states, held)?;
        if edges {
            grow(&mut explored.made, made, held)?;
            grow(&mut explored.edges, ends, held)?;
        }
        let total = explored.states.bytes()
            + explored.packer.bytes()
            + bytes(&explored.firsts)
            + bytes(&explored.made)
            + bytes(&explored.edges)
            + queue.bytes();
        explored.held.set(total)
    }
}

/// The states found and not yet walked, by their index.
trait Frontier {
    /// The state found `index`th, `distance` moves from where the walk heads.
    fn add(&mut self, index: usize, distance: usize);
    /// The state to walk next, of the `found` so far.
    fn take(&mut self, found: usize) -> Option<usize>;
    /// Make room for `spare` more, taking its memory from `held` first.
    fn grow(&mut self, spare: usize, held: &mut Held<'_>) -> Result<(), String>;
    /// Bytes it holds.
    fn bytes(&self) -> usize;
}

/// The first found first: states are kept in the order they were found, so
/// the next is the one after the last walked.
#[derive(Default)]
struct InOrder(usize);

impl Frontier for InOrder {
    fn add(&mut self, _: usize, _: usize) {}
    fn take(&mut self, found: usize) -> Option<usize> {
        let next = (self.0 < found).then_some(self.0)?;
        self.0 += 1;
        Some(next)
    }
    fn grow(&mut self, _: usize, _: &mut Held<'_>) -> Result<(), String> {
        Ok(())
    }
    fn bytes(&self) -> usize {
        0
    }
}

/// The nearest first, and the latest found among the nearest.
impl Frontier for BinaryHeap<(Reverse<usize>, usize)> {
    fn add(&mut self, index: usize, distance: usize) {
        self.push((Reverse(distance), index));
    }
    fn take(&mut self, _: usize) -> Option<usize> {
        self.pop().map(|(_, index)| index)
    }
    fn grow(&mut self, spare: usize, held: &mut Held<'_>) -> Result<(), String> {
        let len = self.len();
        if self.capacity() - len >= spare {
            return Ok(());
        }
        let wanted = (len + spare).max(self.capacity() * 2);
        let entry = size_of::<(Reverse<usize>, usize)>();
        held.add(wanted * entry)?;
        self.reserve_exact(wanted - len);
        held.sub(wanted * entry);
        Ok(())
    }
    fn bytes(&self) -> usize {
        self.capacity() * size_of::<(Reverse<usize>, usize)>()
    }
}
