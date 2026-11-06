//! The senders a kind stands for besides its example: those the partition
//! found answering as some node did and so did not split again.

use super::memory::fact_heap;
use super::split::facts_mut;
use super::*;

/// Names, by their place in [`Alike::names`], one bit each.
type Names = Vec<u64>;

/// A name a split tested, by its place, and the fact it set there.
type Set = (usize, Option<Fact>);

/// The partition's nodes, and the children it dropped for answering as a
/// node it kept. Two that answer alike agree on every value a later split
/// reads (their forms show those values), so a dropped child's senders are
/// split as the node it answers as is. A child's example is its parent's
/// with one fact set, so a node keeps only that fact, and a dropped child
/// only the names where its example and the node's differ.
pub(super) struct Alike {
    /// Each name a split tested: the type, or an attribute or context key.
    names: Vec<Test>,
    /// Each node kept: the node it was split from, the name tested there
    /// and the fact it took (none for the root).
    nodes: Vec<(usize, Option<Set>)>,
    /// Each child dropped: the node it was split from, the name tested, the
    /// names where it and the node it answers as differ, and that node.
    dropped: Vec<(usize, usize, Names, usize)>,
    /// What the facts and name sets above hold on the heap.
    heap: usize,
}

impl Alike {
    /// The node every sender starts from, whose example knows nothing.
    pub(super) const ROOT: usize = 0;

    pub(super) fn new() -> Self {
        Self {
            names: Vec::new(),
            nodes: vec![(Self::ROOT, None)],
            dropped: Vec::new(),
            heap: 0,
        }
    }

    /// A node kept, split from `parent` on `test`.
    pub(super) fn node(&mut self, parent: usize, test: &Test, profile: &Profile) -> usize {
        let name = self.name(test);
        let fact = fact_at(profile, &self.names[name]);
        self.heap += fact.as_ref().map_or(0, fact_heap);
        self.nodes.push((parent, Some((name, fact))));
        self.nodes.len() - 1
    }

    /// A child of `parent` on `test` dropped for answering as the node `like`.
    pub(super) fn drop(&mut self, parent: usize, test: &Test, profile: &Profile, like: usize) {
        let name = self.name(test);
        let example = self.example(like);
        let mut differ = Names::new();
        for (at, name) in self.names.iter().enumerate() {
            if fact_at(profile, name) != fact_at(&example, name) {
                set(&mut differ, at);
            }
        }
        self.heap += differ.capacity() * size_of::<u64>();
        self.dropped.push((parent, name, differ, like));
    }

    /// The bytes this holds, and what [`Self::blurred`] would make of it.
    pub(super) fn bytes(&self) -> usize {
        let names = self.names.capacity() * size_of::<Test>()
            + self
                .names
                .iter()
                .map(|test| match test {
                    Test::Named(_, name) => name.capacity(),
                    _ => 0,
                })
                .sum::<usize>();
        // A set per node and one more being built; each node's list of the
        // children dropped into it, which holds at least four once it holds
        // one and at most twice what it holds.
        let set = size_of::<Names>() + self.names.len().div_ceil(64) * size_of::<u64>();
        let into = self.nodes.len() * size_of::<Vec<usize>>()
            + (4 * self.nodes.len().min(self.dropped.len()) + 2 * self.dropped.len())
                * size_of::<usize>();
        let blurred = (self.nodes.len() + 1) * set + into;
        self.nodes.capacity() * size_of::<(usize, Option<Set>)>()
            + self.dropped.capacity() * size_of::<(usize, usize, Names, usize)>()
            + self.heap
            + names
            + blurred
    }

    /// The names each node's other senders may hold otherwise than its
    /// example: those of the node it was split from but the one tested
    /// there, and for each child dropped for answering as it, where that
    /// child's example differs from the node's and that child's own. They
    /// agree with the node on every value its later splits read, so a split
    /// tells them apart as it does the example. A dropped child can answer
    /// as a node found before or after its parent, so this runs until no set
    /// grows; sets only grow, within the names, so it ends.
    pub(super) fn blurred(&self) -> Vec<Names> {
        let words = self.names.len().div_ceil(64);
        let mut into: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (at, dropped) in self.dropped.iter().enumerate() {
            into[dropped.3].push(at);
        }
        let mut blurred: Vec<Names> = vec![vec![0; words]; self.nodes.len()];
        let mut changed = true;
        while changed {
            changed = false;
            for at in 0..self.nodes.len() {
                let mut found = blurred[at].clone();
                if let (parent, Some((name, _))) = &self.nodes[at] {
                    add(&mut found, &blurred[*parent], Some(*name));
                }
                for &d in &into[at] {
                    let (from, name, differ, _) = &self.dropped[d];
                    add(&mut found, differ, None);
                    add(&mut found, &blurred[*from], Some(*name));
                }
                if found != blurred[at] {
                    blurred[at] = found;
                    changed = true;
                }
            }
        }
        blurred
    }

    /// A profile of every sender `example` stands for with `blurred` names
    /// held otherwise, or none when it stands for no others.
    pub(super) fn others(&self, example: &Profile, blurred: &Names) -> Option<Profile> {
        let mut others = example.clone();
        let mut any = false;
        for (at, name) in self.names.iter().enumerate() {
            if blurred[at / 64] >> (at % 64) & 1 == 1 {
                set_at(&mut others, name, None);
                any = true;
            }
        }
        any.then_some(others)
    }

    /// The name `test` reads, as a place in [`Self::names`].
    fn name(&mut self, test: &Test) -> usize {
        let test = match test {
            Test::Like(place, name, _) => Test::Named(*place, name.clone()),
            test => test.clone(),
        };
        match self.names.iter().position(|known| *known == test) {
            Some(at) => at,
            None => {
                self.names.push(test);
                self.names.len() - 1
            }
        }
    }

    /// The example of the node `at`: the facts its splits set.
    fn example(&self, mut at: usize) -> Profile {
        let mut set: Vec<&Set> = Vec::new();
        while let (parent, Some(fact)) = &self.nodes[at] {
            set.push(fact);
            at = *parent;
        }
        let mut example = Profile::default();
        for (name, fact) in set.into_iter().rev() {
            set_at(&mut example, &self.names[*name], fact.clone());
        }
        example
    }
}

/// Adds the names in `from`, but `except`, to `onto`.
fn add(onto: &mut Names, from: &Names, except: Option<usize>) {
    for (at, word) in from.iter().enumerate() {
        let mut word = *word;
        if let Some(except) = except.filter(|except| except / 64 == at) {
            word &= !(1 << (except % 64));
        }
        onto[at] |= word;
    }
}

fn set(names: &mut Names, at: usize) {
    if names.len() <= at / 64 {
        names.resize(at / 64 + 1, 0);
    }
    names[at / 64] |= 1 << (at % 64);
}

/// What `profile` holds where `name` reads.
fn fact_at(profile: &Profile, name: &Test) -> Option<Fact> {
    match name {
        Test::Kind => profile.kind.clone(),
        Test::Named(place, name) | Test::Like(place, name, _) => {
            facts(profile, *place).get(name).cloned()
        }
    }
}

fn set_at(profile: &mut Profile, name: &Test, fact: Option<Fact>) {
    match (name, fact) {
        (Test::Kind, fact) => profile.kind = fact,
        (Test::Named(place, name) | Test::Like(place, name, _), Some(fact)) => {
            facts_mut(profile, *place).insert(name.clone(), fact);
        }
        (Test::Named(place, name) | Test::Like(place, name, _), None) => {
            facts_mut(profile, *place).remove(name);
        }
    }
}
