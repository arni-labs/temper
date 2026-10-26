//! Who sends each move, as the kernel sends it, and whether Cedar lets them.
//!
//! - An action sent from outside: whoever sent it, someone outside the kernel.
//! - A reaction with a declared principal: that service, with the trigger's
//!   source in its context (`resolve_trigger_principal` in the trigger
//!   dispatcher). Without one: whoever sent its source action, which for a
//!   reaction from a type outside the model may be any sender.
//! - A call's answer, or its `on_failure`: `service:wasm-runtime` for WASM
//!   and `service:platform-dispatch` for an adapter in the background,
//!   and the caller when it ran inline
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
//!
//! What each sender may send is read from the policies once, when a walk
//! first asks: it asks about few of the moves, and reading each answer takes
//! far longer than a step (on Katagami control, every answer up front took
//! 3.4 s for a walk of 126 states). A reaction
//! whose answer depends on the request is decided again when it arrives,
//! with what the joint state knows of it: its source's id, and its record's
//! id and tracked fields. A reaction that finds its record through a field
//! (`resolve_target` `field`) needs this, since only the state says whether
//! that record is one its source may act on.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use serde_json::Value;
use temper_authz::{Asker, Attr, May, SendAnalysis};
use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};

use super::env::ID;
use super::state::JointState;

/// Someone outside the kernel.
pub(super) const OUTSIDE: u32 = 0;
/// The runtime, answering a module call that ran in the background.
pub(super) const RUNTIME: u32 = 1;
/// The platform dispatcher, answering an adapter call in the background.
pub(super) const PLATFORM_DISPATCH: u32 = 2;

/// The senders a model knows, by number, and what Cedar lets each send.
pub(super) struct Senders {
    askers: Vec<Asker>,
    /// Entity, then action, then each sender's answer, once read.
    may: RefCell<BTreeMap<String, Answers>>,
    /// The policies, to decide again a move whose answer depends on the
    /// request.
    policies: Option<SendAnalysis>,
    /// The answers decided again so far: a walk asks the same few questions
    /// over and over, and reading the policies takes far longer than a step.
    decided: RefCell<HashMap<Question, May>>,
}

/// A proof's digest prints the senders and the policies that decide them
/// (those about the model's actions), not the answers read so far, which
/// depend on what was asked.
impl fmt::Debug for Senders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Senders")
            .field("askers", &self.askers)
            .field("policies", &self.policies)
            .finish()
    }
}

/// Each action's answers by sender, read from the policies when first asked.
type Answers = BTreeMap<String, Vec<Option<May>>>;

/// A move decided again: its sender, record type and action, its source's
/// id, and the record's tracked strings and booleans.
type Question = (
    u32,
    String,
    String,
    Option<String>,
    BTreeMap<String, String>,
    BTreeMap<String, bool>,
);

impl Senders {
    /// The senders of the moves among `entities`, with `also` (the services
    /// reactions from outside them run as), and what `policies` lets each
    /// send.
    pub(super) fn new<'a>(
        by_name: &BTreeMap<&str, &Automaton>,
        entities: impl IntoIterator<Item = &'a String> + Clone,
        also: impl IntoIterator<Item = Asker>,
        policies: Option<&SendAnalysis>,
    ) -> Self {
        let mut askers = vec![
            Asker::Outside,
            service("wasm-runtime", BTreeMap::new()),
            service("platform-dispatch", BTreeMap::new()),
        ];
        let declared_in = entities.clone().into_iter().flat_map(|entity| {
            by_name[entity.as_str()]
                .actions
                .iter()
                .flat_map(move |action| {
                    action
                        .triggers
                        .iter()
                        .filter_map(move |trigger| declared(trigger, entity, &action.name))
                })
        });
        for asker in declared_in.chain(also) {
            if !askers.contains(&asker) {
                askers.push(asker);
            }
        }
        let sent: BTreeSet<&str> = entities
            .clone()
            .into_iter()
            .flat_map(|entity| by_name[entity.as_str()].actions.iter())
            .map(|action| action.name.as_str())
            .collect();
        // What the policies say of other actions decides no move here, and
        // a proof's digest is of what its walk reads.
        let policies = policies.map(|policies| policies.for_actions(|a| sent.contains(a)));
        let may = entities
            .into_iter()
            .map(|entity| {
                let actions = by_name[entity.as_str()]
                    .actions
                    .iter()
                    .map(|action| (action.name.clone(), vec![None; askers.len()]))
                    .collect();
                (entity.clone(), actions)
            })
            .collect();
        Self {
            askers,
            may: RefCell::new(may),
            policies,
            decided: RefCell::default(),
        }
    }

    /// Whether Cedar lets sender `by` send `action` to an `entity` record.
    pub(super) fn may(&self, by: u32, entity: &str, action: &str) -> May {
        let Some(policies) = &self.policies else {
            return May::Yes;
        };
        let mut may = self.may.borrow_mut();
        let Some(answer) = may
            .get_mut(entity)
            .and_then(|actions| actions.get_mut(action))
            .and_then(|answers| answers.get_mut(by as usize))
        else {
            return May::No;
        };
        *answer.get_or_insert_with(|| {
            policies.may(&self.askers[by as usize], action, entity, &BTreeMap::new())
        })
    }

    /// Whether Cedar lets sender `by` send `action` to the `entity` record
    /// of `state`: as the policies alone answer, or, when that depends on
    /// the request, as they answer given the sender's source id and the
    /// record's id and tracked fields in `state`.
    pub(super) fn may_in(&self, by: u32, entity: &str, action: &str, state: &JointState) -> May {
        let may = self.may(by, entity, action);
        let (May::Maybe, Some(policies), Some(asker), Some(target)) = (
            may,
            &self.policies,
            self.askers.get(by as usize),
            state.records.get(entity),
        ) else {
            return may;
        };
        // A declared reaction names its source's type; the kernel adds the
        // source record's id.
        let source_id = match asker {
            Asker::Service { context, .. } => match context.get("triggerSourceEntityType") {
                Some(Attr::Value(Value::String(source))) => state
                    .records
                    .get(source)
                    .and_then(|record| record.strings.get(ID)),
                _ => None,
            },
            Asker::Outside => None,
        };
        let question = (
            by,
            entity.to_string(),
            action.to_string(),
            source_id.cloned(),
            target.strings.clone(),
            target.booleans.clone(),
        );
        if let Some(may) = self.decided.borrow().get(&question) {
            return *may;
        }
        let asker = match (asker, source_id) {
            (Asker::Service { name, context }, Some(id)) => {
                let mut context = context.clone();
                context.insert("triggerSourceEntityId".to_string(), symbol(id));
                service(name, context)
            }
            (asker, _) => asker.clone(),
        };
        let strings = target.strings.iter().map(|(field, value)| {
            let field = if field == ID { "id" } else { field.as_str() };
            (field.to_string(), symbol(value))
        });
        let booleans = target
            .booleans
            .iter()
            .map(|(field, value)| (field.clone(), Attr::Value(Value::Bool(*value))));
        let may = policies.may(&asker, action, entity, &strings.chain(booleans).collect());
        self.decided.borrow_mut().insert(question, may);
        may
    }

    /// The number of the sender a reaction runs as: its declared principal,
    /// or `invoker` without one.
    pub(super) fn reaction(&self, declared_as: Option<&Asker>, invoker: u32) -> u32 {
        declared_as
            .and_then(|asker| self.askers.iter().position(|a| a == asker))
            .map_or(invoker, |at| at as u32)
    }

    /// Every sender's number.
    pub(super) fn all(&self) -> impl Iterator<Item = u32> {
        0..self.askers.len() as u32
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

/// A tracked string as Cedar may read it: `#n`, a value the model made up,
/// is known only by which others it equals; any other is the literal itself.
fn symbol(value: &str) -> Attr {
    if value.starts_with('#') {
        Attr::Same(value.to_string())
    } else {
        Attr::Value(Value::String(value.to_string()))
    }
}

fn service(name: &str, context: BTreeMap<String, Attr>) -> Asker {
    Asker::Service {
        name: name.to_string(),
        context,
    }
}
