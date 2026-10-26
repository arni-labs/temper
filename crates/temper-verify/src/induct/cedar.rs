//! What Cedar lets each sender of a proof's moves send: someone outside, a
//! module's answer, a timer as whoever armed it, a reaction as the sender it
//! declares. Answers are kept, since the moves of each region ask again.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_authz::{Asker, Attr, May, SendAnalysis};
use temper_spec::automaton::{ActionKind, ActionTrigger, TargetResolver};

use super::vocab::Vocab;

impl Vocab<'_> {
    /// What Cedar answers `asker` sending `action` to `entity`.
    fn ask(&self, policies: &SendAnalysis, asker: &Asker, action: &str, entity: &str) -> May {
        let question = format!("{asker:?} {action} {entity}");
        let answered = || self.answered.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(may) = answered().get(&question) {
            return *may;
        }
        let may = policies.may(asker, action, entity, &BTreeMap::new());
        answered().insert(question, may);
        may
    }

    /// Whether Cedar lets someone outside send `action` to `entity`.
    pub(super) fn outside_may(&self, entity: &str, action: &str) -> May {
        self.policies.map_or(May::Yes, |policies| {
            self.ask(policies, &Asker::Outside, action, entity)
        })
    }

    /// Whether Cedar lets a module's answer, run as the runtime, take
    /// `action` on `entity`.
    pub(super) fn runtime_may(&self, entity: &str, action: &str) -> May {
        self.policies.map_or(May::Yes, |policies| {
            self.ask(policies, &runtime(), action, entity)
        })
    }

    /// Whether Cedar lets `entity`'s timer in `state` fire `action`. A
    /// timer fires as whoever armed it: whoever made the record in its
    /// initial state, or sent an action that brought it into `state` or
    /// resets the timer (the joint walk's reading).
    pub(super) fn timer_may(&self, entity: &str, state: &str, action: &str) -> May {
        let Some(policies) = self.policies else {
            return May::Yes;
        };
        let mut askers = Vec::new();
        self.armers(entity, state, &mut BTreeSet::new(), &mut askers);
        let answers: BTreeSet<May> = askers
            .iter()
            .map(|asker| self.ask(policies, asker, action, entity))
            .collect();
        match answers.into_iter().collect::<Vec<_>>().as_slice() {
            [] => May::No,
            [one] => *one,
            _ => May::Maybe,
        }
    }

    /// The senders that may bring `entity`'s record into `state`.
    fn armers(&self, entity: &str, state: &str, seen: &mut BTreeSet<String>, out: &mut Vec<Asker>) {
        if self.policies.is_none() || !seen.insert(state.to_string()) {
            return;
        }
        let automaton = self.automaton(entity);
        let mut answered: BTreeSet<&str> = automaton
            .actions
            .iter()
            .filter(|a| a.kind == ActionKind::Internal)
            .map(|a| a.name.as_str())
            .collect();
        for trigger in automaton.actions.iter().flat_map(|a| &a.triggers) {
            answered.extend(trigger.on_success.iter().map(String::as_str));
            answered.extend(trigger.on_failure.as_deref());
        }
        if automaton.automaton.initial == state {
            out.push(Asker::Outside);
        }
        let resets: BTreeSet<&str> = automaton
            .state_timeouts
            .iter()
            .filter(|t| t.state == state)
            .flat_map(|t| t.reset_on.iter().map(String::as_str))
            .collect();
        let enters = automaton.actions.iter().filter(|a| {
            let from_elsewhere = a.from.is_empty() || a.from.iter().any(|f| f != state);
            (a.to.as_deref() == Some(state) && from_elsewhere) || resets.contains(a.name.as_str())
        });
        for action in enters {
            if action.kind == ActionKind::Input && self.outside_may(entity, &action.name) != May::No
            {
                out.push(Asker::Outside);
            }
            if answered.contains(action.name.as_str()) {
                out.push(runtime());
            }
            for (source, source_automaton) in &self.by_name {
                for source_action in &source_automaton.actions {
                    for trigger in &source_action.triggers {
                        if trigger.target_entity.as_deref() == Some(entity)
                            && trigger.target_action.as_deref() == Some(action.name.as_str())
                        {
                            // Without a principal it runs as whoever sent
                            // its source.
                            out.push(
                                declared(trigger, source, &source_action.name)
                                    .unwrap_or(Asker::Outside),
                            );
                        }
                    }
                }
            }
            for timeout in automaton
                .state_timeouts
                .iter()
                .filter(|t| t.on_timeout == action.name)
            {
                self.armers(entity, &timeout.state, seen, out);
            }
        }
    }

    /// Whether Cedar lets a reaction from outside the scope by `trigger`
    /// on `source.source_action` send its action; without a principal it
    /// runs as whoever sent its source, some of whom it may let.
    pub(super) fn reaction_may(
        &self,
        source: &str,
        source_action: &str,
        trigger: &ActionTrigger,
        to: &str,
        action: &str,
    ) -> May {
        let Some(policies) = self.policies else {
            return May::Yes;
        };
        match declared(trigger, source, source_action) {
            Some(asker) => self.ask(policies, &asker, action, to),
            None => May::Maybe,
        }
    }
}

/// A module's answer, run in the background, as the kernel sends it.
fn runtime() -> Asker {
    Asker::Service {
        name: "wasm-runtime".to_string(),
        context: BTreeMap::new(),
    }
}

/// The sender a reaction declares, as the trigger dispatcher builds it.
fn declared(trigger: &ActionTrigger, source: &str, source_action: &str) -> Option<Asker> {
    let name = trigger.principal.as_deref().filter(|p| !p.is_empty())?;
    let text = |value: &str| Attr::Value(Value::String(value.to_string()));
    let source_id = match trigger.resolve_target {
        Some(TargetResolver::SameId) => Attr::ResourceId,
        _ => Attr::Unknown,
    };
    let context = BTreeMap::from([
        ("triggerRule".to_string(), text(&trigger.name)),
        ("triggerSourceEntityType".to_string(), text(source)),
        ("triggerSourceEntityId".to_string(), source_id),
        ("triggerSourceAction".to_string(), text(source_action)),
        (
            "triggerDeclaredPrincipal".to_string(),
            Attr::Value(Value::Bool(true)),
        ),
    ]);
    Some(Asker::Service {
        name: name.to_string(),
        context,
    })
}
