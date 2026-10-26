//! The walk over joint states: breadth first, or toward a status.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};

use super::super::pack::{Packed, Packer};
use super::super::room::{Bound, Held, MB, block, map_bytes};
use super::super::state::render;
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
    /// The memory its tables hold, given back when it is dropped.
    pub(in crate::composite::joint) held: Held<'a>,
}

impl Explored<'_> {
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
    /// every move between walked states is kept. The walk takes the memory
    /// it holds from `bound`'s room and stops when the room says no.
    pub(in crate::composite::joint) fn walk_with<'a>(
        &self,
        bound: Bound<'a>,
        edges: bool,
        visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored<'a> {
        self.walk_in(bound, edges, VecDeque::new(), |_| 0, visit)
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
        let mut explored = Explored {
            nodes: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
            held: Held::new(bound),
        };
        let mut seen: HashMap<Packed, u32> = HashMap::new();
        let mut tables = Tables::default();
        for init in self.inits() {
            let packed = explored.packer.pack(&init);
            if !seen.contains_key(&packed) {
                seen.insert(packed.clone(), explored.nodes.len() as u32);
                queue.add(explored.nodes.len(), distance(&init));
                explored.nodes.push((packed, None));
            }
        }
        while let Some(index) = queue.take() {
            let state = explored.state(index);
            if visit(index, &state) {
                break;
            }
            explored.held.walked(explored.nodes.len());
            if let Err(reason) = tables.make_room(&mut explored, &mut seen, &mut queue, edges) {
                explored.stopped = Some(format!(
                    "the walk does not fit in the memory it may hold: {reason} \
                     (it held {} MB at {} states)",
                    explored.held.bytes / MB,
                    explored.nodes.len()
                ));
                break;
            }
            let before = [
                explored.nodes.len(),
                explored.made.len(),
                explored.edges.len(),
            ];
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
                            tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
                            seen.insert(packed.clone(), at);
                            explored.nodes.push((packed, Some((index as u32, label))));
                            queue.add(at as usize, distance(&next.state));
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
            let after = [
                explored.nodes.len(),
                explored.made.len(),
                explored.edges.len(),
            ];
            tables.saw([0, 1, 2].map(|i| after[i] - before[i]));
        }
        explored
    }
}

/// The walk's tables as the memory it holds counts them: each at its
/// capacity, and each packed state as its own heap block. Before each state
/// is walked, every table is grown to hold what walking one state has added
/// at most so far, with the memory of its new copy taken first, so a table
/// seldom grows on its own while both copies are held.
#[derive(Default)]
struct Tables {
    /// Bytes the packed states take.
    states: usize,
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
        seen: &mut HashMap<Packed, u32>,
        queue: &mut impl List,
        edges: bool,
    ) -> Result<(), String> {
        let [states, made, ends] = self.most.map(|most| most.max(1));
        let held = &mut explored.held;
        grow(&mut explored.nodes, states, held)?;
        grow(queue, states, held)?;
        grow_map(seen, states, held)?;
        if edges {
            grow(&mut explored.made, made, held)?;
            grow(&mut explored.edges, ends, held)?;
        }
        let bytes = self.states
            + explored.packer.bytes()
            + bytes(&explored.nodes)
            + bytes(queue)
            + map_bytes::<Packed, u32>(seen.capacity())
            + bytes(&explored.made)
            + bytes(&explored.edges);
        explored.held.set(bytes)
    }
}

/// A list the walk keeps.
trait List {
    /// Bytes one entry takes.
    const ENTRY: usize;
    fn len(&self) -> usize;
    fn capacity(&self) -> usize;
    fn reserve_exact(&mut self, more: usize);
}

impl<T> List for Vec<T> {
    const ENTRY: usize = size_of::<T>();
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn capacity(&self) -> usize {
        Vec::capacity(self)
    }
    fn reserve_exact(&mut self, more: usize) {
        Vec::reserve_exact(self, more);
    }
}

impl<T> List for VecDeque<T> {
    const ENTRY: usize = size_of::<T>();
    fn len(&self) -> usize {
        VecDeque::len(self)
    }
    fn capacity(&self) -> usize {
        VecDeque::capacity(self)
    }
    fn reserve_exact(&mut self, more: usize) {
        VecDeque::reserve_exact(self, more);
    }
}

impl<T: Ord> List for BinaryHeap<T> {
    const ENTRY: usize = size_of::<T>();
    fn len(&self) -> usize {
        BinaryHeap::len(self)
    }
    fn capacity(&self) -> usize {
        BinaryHeap::capacity(self)
    }
    fn reserve_exact(&mut self, more: usize) {
        BinaryHeap::reserve_exact(self, more);
    }
}

/// The states found and not yet walked, by their index.
trait Frontier: List {
    fn add(&mut self, index: usize, distance: usize);
    fn take(&mut self) -> Option<usize>;
}

/// The first found first.
impl Frontier for VecDeque<usize> {
    fn add(&mut self, index: usize, _: usize) {
        self.push_back(index);
    }
    fn take(&mut self) -> Option<usize> {
        self.pop_front()
    }
}

/// The nearest first, and the latest found among the nearest.
impl Frontier for BinaryHeap<(Reverse<usize>, usize)> {
    fn add(&mut self, index: usize, distance: usize) {
        self.push((Reverse(distance), index));
    }
    fn take(&mut self) -> Option<usize> {
        self.pop().map(|(_, index)| index)
    }
}

fn bytes<L: List>(list: &L) -> usize {
    list.capacity() * L::ENTRY
}

/// Make `list` hold `spare` more, doubling it when it must grow, with the
/// memory of its new copy taken while the old one is still held.
fn grow<L: List>(list: &mut L, spare: usize, held: &mut Held<'_>) -> Result<(), String> {
    let len = list.len();
    if list.capacity() - len >= spare {
        return Ok(());
    }
    let wanted = (len + spare).max(list.capacity() * 2);
    held.add(wanted * L::ENTRY)?;
    list.reserve_exact(wanted - len);
    held.sub(wanted * L::ENTRY);
    Ok(())
}

/// Make `map` hold `spare` more, as [`grow`] does a list.
fn grow_map(
    map: &mut HashMap<Packed, u32>,
    spare: usize,
    held: &mut Held<'_>,
) -> Result<(), String> {
    let len = map.len();
    if map.capacity() - len >= spare {
        return Ok(());
    }
    let wanted = (len + spare).max(map.capacity() * 2);
    held.add(map_bytes::<Packed, u32>(wanted))?;
    map.reserve(wanted - len);
    held.sub(map_bytes::<Packed, u32>(wanted));
    Ok(())
}
