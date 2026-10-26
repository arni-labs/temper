//! Who sends each move, as the kernel sends it, and whether Cedar lets them.
//!
//! - An action sent from outside: whoever sent it, someone outside the kernel.
//! - A reaction with a declared principal: that service, with the trigger's
//!   source in its context (`resolve_trigger_principal` in the trigger
//!   dispatcher). Without one: whoever sent its source action.
//! - A module's answer, or its `on_failure`: `service:wasm-runtime` when the
//!   call ran in the background, and the caller when it ran inline
//!   (`WasmDispatchMode` in `invocation_artifacts.rs`). Only an outside
//!   caller can ask for inline (`?await_integration`); the kernel's own
//!   dispatches never do.
//! - A state timer: whoever armed it, the sender of the move that entered its
//!   state or last reset it (`timer_agent` in `state_timeouts.rs`).
//!
//! Cedar decides two of these: a request from outside, at its entry point,
//! and a reaction, as its sender (the trigger dispatcher). A timer's move and
//! a module's answer are dispatched without it (`dispatch` keeps
//! authorization at the entry points), so their sender matters for the
//! reactions they set off. Without policies every sender may send everything.

use std::collections::BTreeMap;

use serde_json::Value;
use temper_authz::{Asker, Attr, May, SendAnalysis};
use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};

/// Someone outside the kernel.
pub(super) const OUTSIDE: u32 = 0;
/// The runtime, answering a module call that ran in the background.
pub(super) const RUNTIME: u32 = 1;

/// The senders a model knows, by number, and what Cedar lets each send.
pub(super) struct Senders {
    askers: Vec<Asker>,
    /// Each sender with resource-relative IDs forgotten after a different-ID hop.
    detached: Vec<u32>,
    /// Entity, then action, then each sender's answer.
    may: Option<BTreeMap<String, BTreeMap<String, Vec<May>>>>,
}

impl Senders {
    /// The senders of the moves among `entities`, and what `policies` lets
    /// each send.
    pub(super) fn new<'a>(
        by_name: &BTreeMap<&str, &Automaton>,
        entities: impl IntoIterator<Item = &'a String> + Clone,
        policies: Option<&SendAnalysis>,
    ) -> Self {
        let mut askers = vec![Asker::Outside, service("wasm-runtime", BTreeMap::new())];
        for entity in entities.clone() {
            for action in &by_name[entity.as_str()].actions {
                for trigger in &action.triggers {
                    let Some(asker) = declared(trigger, entity, &action.name) else {
                        continue;
                    };
                    if !askers.contains(&asker) {
                        askers.push(asker);
                    }
                }
            }
        }
        // ResourceId is relative to the current receiver, not a stable ID.
        // Precompute a closed set of senders for chains that change receiver ID.
        let mut detached = Vec::new();
        for mut asker in askers.clone() {
            if let Asker::Service { context, .. } = &mut asker {
                for attr in context.values_mut() {
                    if *attr == Attr::ResourceId {
                        *attr = Attr::Unknown;
                    }
                }
            }
            let at = askers.iter().position(|a| a == &asker).unwrap_or_else(|| {
                askers.push(asker);
                askers.len() - 1
            });
            detached.push(at as u32);
        }
        detached.extend((detached.len()..askers.len()).map(|at| at as u32));
        let may = policies.map(|policies| {
            entities
                .into_iter()
                .map(|entity| {
                    let actions = by_name[entity.as_str()]
                        .actions
                        .iter()
                        .map(|action| {
                            let answers = askers
                                .iter()
                                .map(|asker| policies.may(asker, &action.name, entity))
                                .collect();
                            (action.name.clone(), answers)
                        })
                        .collect();
                    (entity.clone(), actions)
                })
                .collect()
        });
        Self {
            askers,
            detached,
            may,
        }
    }

    /// Whether Cedar lets sender `by` send `action` to an `entity` record.
    pub(super) fn may(&self, by: u32, entity: &str, action: &str) -> May {
        let Some(may) = &self.may else {
            return May::Yes;
        };
        may.get(entity)
            .and_then(|actions| actions.get(action))
            .and_then(|answers| answers.get(by as usize))
            .copied()
            .unwrap_or(May::No)
    }

    /// The number of the sender a reaction runs as: its declared principal,
    /// or `invoker` without one. An inherited resource-relative ID remains
    /// known only when this hop preserves the receiver ID.
    pub(super) fn reaction(&self, declared_as: Option<&Asker>, invoker: u32, same_id: bool) -> u32 {
        declared_as
            .and_then(|asker| self.askers.iter().position(|a| a == asker))
            .map_or_else(
                || {
                    if same_id {
                        invoker
                    } else {
                        self.detached[invoker as usize]
                    }
                },
                |at| at as u32,
            )
    }

    /// Who sender `by` is, in words.
    pub(super) fn name(&self, by: u32) -> String {
        match self.askers.get(by as usize) {
            Some(Asker::Outside) => "an outside caller".to_string(),
            Some(Asker::Service { name, .. }) => format!("service:{name}"),
            None => format!("sender {by}"),
        }
    }
}

/// The sender a reaction declares, as the trigger dispatcher builds it.
pub(super) fn declared(
    trigger: &temper_spec::automaton::ActionTrigger,
    source_entity: &str,
    source_action: &str,
) -> Option<Asker> {
    if trigger.kind != TriggerKind::Entity {
        return None;
    }
    let name = trigger.principal.as_deref().filter(|p| !p.is_empty())?;
    let text = |value: &str| Attr::Value(Value::String(value.to_string()));
    // A same-id reaction acts on the record with its source's id.
    let source_id = match trigger.resolve_target {
        Some(TargetResolver::SameId) => Attr::ResourceId,
        _ => Attr::Unknown,
    };
    let context = BTreeMap::from([
        ("triggerRule".to_string(), text(&trigger.name)),
        ("triggerSourceEntityType".to_string(), text(source_entity)),
        ("triggerSourceEntityId".to_string(), source_id),
        ("triggerSourceAction".to_string(), text(source_action)),
        (
            "triggerDeclaredPrincipal".to_string(),
            Attr::Value(Value::Bool(true)),
        ),
    ]);
    Some(service(name, context))
}

fn service(name: &str, context: BTreeMap<String, Attr>) -> Asker {
    Asker::Service {
        name: name.to_string(),
        context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::automaton::parse_automaton;

    #[test]
    fn inherited_source_id_does_not_become_a_different_targets_id() {
        // A[id1] -> B[id1] declares the service. B then forwards the
        // principal to C[id2] through a field, retaining A's id1 at runtime.
        let a = parse_automaton(
            r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "first"
kind = "entity"
target_entity = "B"
target_action = "Forward"
principal = "chain"
[action.triggers.resolve_target]
kind = "same_id"
"#,
        )
        .unwrap();
        let policy = SendAnalysis::new(
            r#"
permit(principal == Agent::"service:chain", action, resource) when {
    context has triggerSourceEntityId && resource has id &&
    context.triggerSourceEntityId == resource.id
};
"#,
        )
        .unwrap();
        let entities = vec!["A".to_string()];
        let senders = Senders::new(&BTreeMap::from([("A", &a)]), &entities, Some(&policy));
        let principal = declared(&a.actions[0].triggers[0], "A", "Start").unwrap();
        let b = senders.reaction(Some(&principal), OUTSIDE, true);
        assert_eq!(senders.may(b, "A", "Start"), May::Yes);
        let c = senders.reaction(None, b, false);
        assert_eq!(senders.may(c, "A", "Start"), May::Maybe);
        // More same-ID hops cannot restore knowledge of the original ID.
        assert_eq!(senders.reaction(None, c, true), c);
        assert_eq!(senders.reaction(None, c, false), c);
        // Same-ID chains preserve knowledge; a declaration starts fresh.
        assert_eq!(senders.reaction(None, b, true), b);
        assert_eq!(senders.reaction(Some(&principal), c, true), b);
    }
}
