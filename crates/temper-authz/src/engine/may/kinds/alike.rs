//! The senders a kind stands for besides its example: those the partition
//! found answering as some node did and so did not split again.

use super::split::facts_mut;
use super::*;

/// The partition's nodes, and the children it dropped for answering as a
/// node it kept. Two that answer alike agree on every value a later split
/// reads (their forms show those values), so a dropped child's senders are
/// split as the node it answers as is.
pub(super) struct Alike {
    /// Each node kept: the node it was split from and the test (none for
    /// the root), and its example.
    nodes: Vec<(usize, Option<Test>, Profile)>,
    /// Each child dropped: the node it was split from, the test, its
    /// example, and the node it answers as.
    dropped: Vec<(usize, Test, Profile, usize)>,
}

impl Alike {
    /// The node every sender starts from.
    pub(super) const ROOT: usize = 0;

    pub(super) fn new(root: &Profile) -> Self {
        Self {
            nodes: vec![(Self::ROOT, None, root.clone())],
            dropped: Vec::new(),
        }
    }

    /// A node kept, split from `parent` on `test`.
    pub(super) fn node(&mut self, parent: usize, test: &Test, profile: &Profile) -> usize {
        self.nodes
            .push((parent, Some(test.clone()), profile.clone()));
        self.nodes.len() - 1
    }

    /// A child of `parent` on `test` dropped for answering as the node `like`.
    pub(super) fn drop(&mut self, parent: usize, test: &Test, profile: Profile, like: usize) {
        self.dropped.push((parent, test.clone(), profile, like));
    }

    /// What this holds, each entry at most a partition node's worth.
    pub(super) fn held(&self) -> usize {
        self.nodes.len() + self.dropped.len()
    }

    /// What each node's other senders hold, one profile wide enough for
    /// all: those of the node it was split from, given its value on the
    /// test, and each child dropped for answering as it, with that child's
    /// own others. They agree with the node on every value its later splits
    /// read, so the value a split copies in is theirs. Widening blurs what
    /// told them apart from the example, which can be what an asked question
    /// read before it was decided: a kind reads these only for a question
    /// its split did not ask ([`Kind::profiles`]). A dropped child can answer
    /// as a node found before or after its parent, so this runs until
    /// nothing widens; a profile only widens, and only so far, so it ends.
    pub(super) fn others(&self) -> Vec<Option<Profile>> {
        let mut into: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (at, dropped) in self.dropped.iter().enumerate() {
            into[dropped.3].push(at);
        }
        let mut others: Vec<Option<Profile>> = vec![None; self.nodes.len()];
        let mut changed = true;
        while changed {
            changed = false;
            for at in 0..self.nodes.len() {
                let (parent, test, own) = &self.nodes[at];
                let mut found = others[at].clone();
                let mut add = |profile: &Profile| {
                    found = Some(match found.take() {
                        Some(found) => widen(&found, profile),
                        None => profile.clone(),
                    });
                };
                if let (Some(test), Some(above)) = (test, &others[*parent]) {
                    let mut above = above.clone();
                    decided(test, own, &mut above);
                    add(&above);
                }
                for &d in &into[at] {
                    let (from, test, profile, _) = &self.dropped[d];
                    add(profile);
                    if let Some(above) = &others[*from] {
                        let mut above = above.clone();
                        decided(test, profile, &mut above);
                        add(&above);
                    }
                }
                if found != others[at] {
                    others[at] = found;
                    changed = true;
                }
            }
        }
        others
    }
}

/// A profile of every sender either describes. A value they hold alike
/// stays; one they differ on widens to what covers both, or unknown.
fn widen(one: &Profile, other: &Profile) -> Profile {
    let facts = |one: &BTreeMap<String, Fact>, other: &BTreeMap<String, Fact>| {
        one.iter()
            .filter_map(|(name, fact)| {
                let wide = either(fact, other.get(name)?)?;
                Some((name.clone(), wide))
            })
            .collect()
    };
    Profile {
        kind: match (&one.kind, &other.kind) {
            (Some(one), Some(other)) => either(one, other),
            _ => None,
        },
        attrs: facts(&one.attrs, &other.attrs),
        context: facts(&one.context, &other.context),
        stand_ins: one.stand_ins.union(&other.stand_ins).cloned().collect(),
    }
}

/// A fact holding every value either holds, or None (unknown) when only
/// that does: a fact cannot say "absent, or some value".
fn either(one: &Fact, other: &Fact) -> Option<Fact> {
    let some = |unlike: Vec<Json>, like: BTreeMap<String, bool>, string| Fact::Other {
        unlike,
        like,
        string,
    };
    match (one, other) {
        _ if one == other => Some(one.clone()),
        (Fact::Absent, _) | (_, Fact::Absent) => None,
        (Fact::Is(value), Fact::Other { unlike, .. })
        | (Fact::Other { unlike, .. }, Fact::Is(value)) => {
            let unlike = unlike
                .iter()
                .filter(|other| *other != value)
                .cloned()
                .collect();
            Some(some(unlike, BTreeMap::new(), None))
        }
        (Fact::Is(_), Fact::Is(_)) => Some(some(Vec::new(), BTreeMap::new(), None)),
        (
            Fact::Other {
                unlike,
                like,
                string,
            },
            Fact::Other {
                unlike: others,
                like: other_like,
                string: other_string,
            },
        ) => Some(some(
            unlike
                .iter()
                .filter(|value| others.contains(value))
                .cloned()
                .collect(),
            like.iter()
                .filter(|(pattern, matches)| other_like.get(*pattern) == Some(matches))
                .map(|(pattern, matches)| (pattern.clone(), *matches))
                .collect(),
            if string == other_string {
                *string
            } else {
                None
            },
        )),
    }
}

/// Gives `onto` what `own` holds where `test` reads.
fn decided(test: &Test, own: &Profile, onto: &mut Profile) {
    match test {
        Test::Kind => onto.kind.clone_from(&own.kind),
        Test::Named(place, name) | Test::Like(place, name, _) => {
            match facts(own, *place).get(name) {
                Some(fact) => {
                    facts_mut(onto, *place).insert(name.clone(), fact.clone());
                }
                None => {
                    facts_mut(onto, *place).remove(name);
                }
            }
        }
    }
}
