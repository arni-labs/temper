//! What Cedar lets each sender send, read from the policies once.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::{Asker, May, SendAnalysis};
use temper_spec::automaton::Automaton;

/// `answer` for each action of each of `entities`, by entity, then action.
pub(super) fn each<'a, T>(
    by_name: &BTreeMap<&str, &Automaton>,
    entities: impl IntoIterator<Item = &'a String>,
    answer: &dyn Fn(&str, &str) -> T,
) -> BTreeMap<String, BTreeMap<String, T>> {
    entities
        .into_iter()
        .map(|entity| {
            let actions = by_name[entity.as_str()]
                .actions
                .iter()
                .map(|action| (action.name.clone(), answer(entity, &action.name)))
                .collect();
            (entity.clone(), actions)
        })
        .collect()
}

/// The policies that may apply to a move whose answer `may` leaves to the
/// request: all a walk reads of `policies` when it decides one again.
pub(super) fn reread(
    policies: Option<&SendAnalysis>,
    askers: &[Asker],
    may: Option<&BTreeMap<String, BTreeMap<String, Vec<May>>>>,
) -> BTreeSet<String> {
    let mut reread = BTreeSet::new();
    if let (Some(policies), Some(may)) = (policies, may) {
        for (entity, actions) in may {
            for (action, answers) in actions {
                for (by, answer) in answers.iter().enumerate() {
                    if *answer == May::Maybe {
                        reread.extend(policies.applicable(&askers[by / 2], action, entity));
                    }
                }
            }
        }
    }
    reread
}
