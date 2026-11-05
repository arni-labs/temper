//! Who a reaction from a type outside the scope runs as: whoever may send
//! the move that set it off. The model holds no record of that move, so
//! this reads the whole app once: who may send each move as a request, and
//! how reactions, spawns, module answers and timers carry a sender on, as
//! [`Senders`] says the kernel does. A service the policies were not read
//! for, and a move nothing reads as sent, stay with the sender the model
//! does not know, which Cedar may answer either way for.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::May;
use temper_spec::automaton::{Action, Automaton, TargetResolver, TriggerKind};

use super::super::super::senders::{OUTSIDE, Senders, declared};
use super::super::read::Kernel;
use super::super::spawn::spawns_of;

/// Senders by move (entity, action).
type ByMove = BTreeMap<(String, String), BTreeSet<u32>>;

/// The senders each reaction from a type outside `scope` into a move in it
/// may run as, by that move. `relies_on` names the moves outside code is
/// relied on to send, whoever sends them.
pub(super) fn sent_as(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    senders: &Senders,
    relies_on: &BTreeSet<(String, String)>,
) -> ByMove {
    let all = by_name.keys().map(|name| name.to_string()).collect();
    let kernel = Kernel::read(by_name, &all);
    let mut sent = ByMove::new();
    for (entity, automaton) in by_name {
        for action in &automaton.actions {
            let key = (entity.to_string(), action.name.clone());
            let relied = relies_on.contains(&key);
            let may = senders.outside().iter().copied().filter(|&by| {
                relied || senders.request_anywhere(by, entity, &action.name) != May::No
            });
            sent.entry(key).or_default().extend(may);
        }
        // A record that starts in a state with a timer has it armed by no
        // one the model knows of: someone outside.
        for timeout in &kernel.timers[*entity] {
            if timeout.state == automaton.automaton.initial {
                let key = (entity.to_string(), timeout.action.clone());
                sent.entry(key).or_default().insert(senders.timer(OUTSIDE));
            }
        }
    }
    loop {
        let mut grew = false;
        for (entity, automaton) in by_name {
            for action in &automaton.actions {
                let key = (entity.to_string(), action.name.clone());
                let from: Vec<u32> = sent[&key].iter().copied().collect();
                for by in from {
                    for (to, next, _) in carried(automaton, entity, action, &kernel, senders, by) {
                        grew |= sent.entry(to).or_default().insert(next);
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    let unknown = [senders.unknown(), senders.timer(senders.unknown())];
    let mut into = ByMove::new();
    for (entity, automaton) in by_name.iter().filter(|(name, _)| !scope.contains(**name)) {
        for action in &automaton.actions {
            let from = &sent[&(entity.to_string(), action.name.clone())];
            let starts: Vec<u32> = if from.is_empty() {
                unknown.to_vec()
            } else {
                from.iter().copied().collect()
            };
            for by in starts {
                for (to, next, reaction) in carried(automaton, entity, action, &kernel, senders, by)
                {
                    if reaction && scope.contains(&to.0) {
                        into.entry(to).or_default().insert(next);
                    }
                }
            }
        }
    }
    into
}

/// The moves `entity`'s `action`, sent by `by`, sets off, with whom each
/// runs as, and whether it is a reaction or spawn rather than a module's
/// answer or a timer.
fn carried<'a>(
    automaton: &'a Automaton,
    entity: &'a str,
    action: &'a Action,
    kernel: &'a Kernel,
    senders: &'a Senders,
    by: u32,
) -> impl Iterator<Item = ((String, String), u32, bool)> + 'a {
    let triggered = action.triggers.iter().filter_map(move |trigger| {
        let (TriggerKind::Entity, Some(to), Some(target)) = (
            &trigger.kind,
            &trigger.target_entity,
            &trigger.target_action,
        ) else {
            return None;
        };
        let next = match declared(trigger, entity, &action.name) {
            Some(service) => senders.declared_as(&service, by),
            None => {
                let same_id = matches!(trigger.resolve_target, Some(TargetResolver::SameId));
                senders.reaction(None, by, same_id)
            }
        };
        Some(((to.clone(), target.clone()), next, true))
    });
    let spawned = spawns_of(action).map(move |(to, target, ..)| {
        (
            (to.clone(), target.clone()),
            senders.reaction(None, by, false),
            true,
        )
    });
    let calls = kernel
        .calls_on
        .get(&(entity.to_string(), action.name.clone()))
        .into_iter()
        .flatten();
    let answers = calls.flat_map(move |spec| {
        let named: Vec<&String> = spec.on_success.iter().chain(&spec.on_failure).collect();
        let names: Vec<&String> = if spec.on_success.is_empty() {
            automaton
                .actions
                .iter()
                .map(|a| &a.name)
                .chain(named)
                .collect()
        } else {
            named
        };
        names
            .into_iter()
            .map(move |name| ((entity.to_string(), name.clone()), by, false))
    });
    let timers = kernel.timers.get(entity).into_iter().flatten();
    let fired = timers
        .filter(move |timeout| {
            action.to.as_deref() == Some(timeout.state.as_str())
                || timeout.reset_on.contains(&action.name)
        })
        .map(move |timeout| {
            let next = senders.timer(Senders::armer(by));
            ((entity.to_string(), timeout.action.clone()), next, false)
        });
    triggered.chain(spawned).chain(answers).chain(fired)
}
