//! What the joint model reads once from the specs: the reactions between
//! the types in scope, the module calls their actions start, their internal
//! actions and their state timers.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::Asker;
use temper_spec::automaton::{ActionKind, Automaton, TargetResolver, TriggerKind};
use temper_spec::predicate::{Arg, Expr};

use super::super::senders;

/// An entity reaction between two types in scope, with the arguments it
/// passes (target parameter to a source field or a literal).
#[derive(Debug, Clone)]
pub(super) struct Reaction {
    pub(super) from: String,
    pub(super) source_action: String,
    pub(super) to: String,
    pub(super) target_action: String,
    pub(super) guard: Option<Expr>,
    pub(super) args: BTreeMap<String, Arg>,
    pub(super) resolve: Option<TargetResolver>,
    /// The service it runs as, when it declares one.
    pub(super) principal: Option<Asker>,
}

/// A module call an action's trigger starts.
#[derive(Debug, Clone)]
pub(super) struct CallSpec {
    pub(super) module: String,
    pub(super) on_success: Option<String>,
    pub(super) on_failure: Option<String>,
}

/// A state timer: in `state`, it fires `action`; `reset_on` arms it again.
#[derive(Debug, Clone)]
pub(in crate::composite::joint) struct Timeout {
    pub(in crate::composite::joint) state: String,
    pub(in crate::composite::joint) action: String,
    pub(in crate::composite::joint) reset_on: Vec<String>,
}

/// What the kernel sends, read once from the specs.
#[derive(Default)]
pub(super) struct Kernel {
    /// Each entity's state timers.
    pub(super) timers: BTreeMap<String, Vec<Timeout>>,
    /// The module calls each (entity, action) starts.
    pub(super) calls_on: BTreeMap<(String, String), Vec<CallSpec>>,
    /// Each entity's internal actions: what a module whose trigger declares
    /// no `on_success` may answer with.
    pub(super) internal: BTreeMap<String, BTreeSet<String>>,
}

impl Kernel {
    /// The timers and module calls of the types in `scope`.
    pub(super) fn read(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> Self {
        let mut kernel = Kernel::default();
        for entity in scope {
            let automaton = by_name[entity.as_str()];
            for action in &automaton.actions {
                for trigger in &action.triggers {
                    let (TriggerKind::Wasm, Some(module)) = (&trigger.kind, &trigger.module) else {
                        continue;
                    };
                    kernel
                        .calls_on
                        .entry((entity.clone(), action.name.clone()))
                        .or_default()
                        .push(CallSpec {
                            module: module.clone(),
                            on_success: trigger.on_success.clone(),
                            on_failure: trigger.on_failure.clone(),
                        });
                }
            }
            kernel.internal.insert(
                entity.clone(),
                automaton
                    .actions
                    .iter()
                    .filter(|a| a.kind == ActionKind::Internal)
                    .map(|a| a.name.clone())
                    .collect(),
            );
            kernel.timers.insert(
                entity.clone(),
                automaton
                    .state_timeouts
                    .iter()
                    .map(|timeout| Timeout {
                        state: timeout.state.clone(),
                        action: timeout.on_timeout.clone(),
                        reset_on: timeout.reset_on.clone(),
                    })
                    .collect(),
            );
        }
        kernel
    }
}

/// The entity reactions whose source and target are both in `scope`.
pub(super) fn reactions_in(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
) -> Vec<Reaction> {
    let mut reactions = Vec::new();
    for from in scope {
        for action in &by_name[from.as_str()].actions {
            for trigger in &action.triggers {
                if trigger.kind != TriggerKind::Entity {
                    continue;
                }
                let (Some(to), Some(target_action)) =
                    (&trigger.target_entity, &trigger.target_action)
                else {
                    continue;
                };
                if !scope.contains(to) {
                    continue;
                }
                reactions.push(Reaction {
                    from: from.clone(),
                    source_action: action.name.clone(),
                    to: to.clone(),
                    target_action: target_action.clone(),
                    guard: trigger.guard.clone(),
                    args: trigger.args.clone(),
                    resolve: trigger.resolve_target.clone(),
                    principal: senders::declared(trigger, from, &action.name),
                });
            }
        }
    }
    reactions
}
