//! What an inductive proof of a rule speaks of: the records of the types the
//! rule names, one per type with the same id, the dispatches in flight
//! between them by kind, and what Cedar lets each sender send.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use temper_authz::{May, SendAnalysis};
use temper_spec::automaton::{
    Action, ActionParam, ActionTrigger, Automaton, TriggerKind, VarType, call_may_answer,
};
use temper_spec::predicate::{Arg, Effect, Literal};

/// A reaction's target in the scope, its action, and the parameters it
/// carries with their sorts.
type Carried = (String, String, Vec<(String, Sort)>);

/// A value's sort in the proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sort {
    /// A string, known only by which others and which literals it equals.
    Str,
    /// An integer; a counter is one that is never negative.
    Int {
        counter: bool,
    },
    Bool,
}

impl Sort {
    pub(super) fn of(var_type: &VarType) -> Option<Sort> {
        match var_type {
            VarType::String => Some(Sort::Str),
            VarType::Bool => Some(Sort::Bool),
            VarType::Counter => Some(Sort::Int { counter: true }),
            VarType::Int => Some(Sort::Int { counter: false }),
            VarType::List => None,
        }
    }

    pub(super) fn smt(self) -> &'static str {
        match self {
            Sort::Str => "Str",
            Sort::Int { .. } => "Int",
            Sort::Bool => "Bool",
        }
    }
}

/// A kind of dispatch: reactions to one action that carry the same
/// parameters, run as the same sender and that Cedar answers the same. A
/// scheduled action waiting to fire is one too, of its own, carrying no
/// parameters.
#[derive(Debug, Clone)]
pub(super) struct Kind {
    /// Its name in the proof: the target and action, numbered when several
    /// kinds share them.
    pub(super) name: String,
    pub(super) to: String,
    pub(super) action: String,
    pub(super) principal: Option<String>,
    /// The parameters it carries, with their sorts.
    pub(super) fields: Vec<(String, Sort)>,
    /// What Cedar answers when it arrives.
    pub(super) may: May,
    /// Whether it is a scheduled action. Like a state timer it may wait as
    /// long as it likes, so a record waiting for one is settled.
    pub(super) scheduled: bool,
}

impl Kind {
    /// A kind, named when it is added to the vocabulary.
    fn new(
        to: String,
        action: String,
        principal: Option<String>,
        fields: Vec<(String, Sort)>,
        may: May,
        scheduled: bool,
    ) -> Self {
        Self {
            name: String::new(),
            to,
            action,
            principal,
            fields,
            may,
            scheduled,
        }
    }

    /// Whether `other` is the same kind, whatever their names.
    fn same(&self, other: &Kind) -> bool {
        self.to == other.to
            && self.action == other.action
            && self.principal == other.principal
            && self.may == other.may
            && self.scheduled == other.scheduled
            && self
                .fields
                .iter()
                .map(|(f, _)| f)
                .eq(other.fields.iter().map(|(f, _)| f))
    }
}

/// A kind of module call: the calls a record's triggers start on one
/// module or adapter with the same kind, declared answer and failure.
/// Returned parameters are chosen independently of call-time fields.
#[derive(Debug, Clone)]
pub(super) struct Call {
    /// Its name in the proof: the record's type and the module, numbered
    /// when several kinds share them.
    pub(super) name: String,
    pub(super) to: String,
    pub(super) module: String,
    pub(super) on_success: Option<String>,
    pub(super) on_failure: Option<String>,
    pub(super) kind: TriggerKind,
}

/// What is in flight, by kind: a dispatch or a module call, and the values
/// each one carries.
pub(super) struct Flight<'v> {
    pub(super) name: &'v str,
    pub(super) fields: Vec<(String, Sort)>,
}

/// The vocabulary of one rule's proof.
pub(super) struct Vocab<'a> {
    pub(super) by_name: BTreeMap<&'a str, &'a Automaton>,
    pub(super) scope: BTreeSet<String>,
    /// Each record's variables the proof holds, with their sorts.
    pub(super) vars: BTreeMap<String, BTreeMap<String, Sort>>,
    pub(super) kinds: Vec<Kind>,
    pub(super) calls: Vec<Call>,
    pub(super) policies: Option<&'a SendAnalysis>,
    /// Cedar's answers so far, by question: the moves of each region ask
    /// the same ones again.
    pub(super) answered: &'a Mutex<BTreeMap<String, May>>,
}

impl<'a> Vocab<'a> {
    pub(super) fn new(
        by_name: BTreeMap<&'a str, &'a Automaton>,
        scope: BTreeSet<String>,
        policies: Option<&'a SendAnalysis>,
        answered: &'a Mutex<BTreeMap<String, May>>,
    ) -> Self {
        let mut vars = BTreeMap::new();
        for entity in &scope {
            let automaton = by_name[entity.as_str()];
            let held: BTreeMap<String, Sort> = automaton
                .state
                .iter()
                .filter_map(|var| Sort::of(&var.var_type).map(|sort| (var.name.clone(), sort)))
                .collect();
            vars.insert(entity.clone(), held);
        }
        let mut vocab = Self {
            by_name,
            scope,
            vars,
            kinds: Vec::new(),
            calls: Vec::new(),
            policies,
            answered,
        };
        vocab.kinds = vocab.read_kinds();
        vocab.calls = vocab.read_calls();
        vocab
    }

    pub(super) fn automaton(&self, entity: &str) -> &'a Automaton {
        self.by_name[entity]
    }

    pub(super) fn action(&self, entity: &str, action: &str) -> Option<&'a Action> {
        self.automaton(entity)
            .actions
            .iter()
            .find(|a| a.name == action)
    }

    /// The sort of `entity`'s variable `var`, when the proof holds it.
    pub(super) fn var(&self, entity: &str, var: &str) -> Option<Sort> {
        self.vars.get(entity)?.get(var).copied()
    }

    /// The sort of `action`'s parameter `param`.
    pub(super) fn param(&self, action: &Action, param: &str) -> Option<Sort> {
        action.params.iter().find_map(|p| match p {
            ActionParam::Named(name) if name == param => Some(Sort::Str),
            ActionParam::Typed {
                name, param_type, ..
            } if name == param => Sort::of(param_type),
            _ => None,
        })
    }

    /// The kind a reaction from `source.action` by `trigger` sends, when its
    /// target is in the scope.
    pub(super) fn kind_of(
        &self,
        source: &str,
        action: &Action,
        trigger: &ActionTrigger,
    ) -> Option<&Kind> {
        let (to, target_action, fields) = self.carried(source, trigger)?;
        let may = self.reaction_may(source, &action.name, trigger, &to, &target_action);
        let probe = Kind::new(
            to,
            target_action,
            trigger.principal.clone(),
            fields,
            may,
            false,
        );
        self.kinds.iter().find(|kind| kind.same(&probe))
    }

    /// The kind `entity`'s action `arming` arms when it schedules `action`.
    pub(super) fn scheduled_kind(&self, entity: &str, arming: &str, action: &str) -> Option<&Kind> {
        let may = self.scheduled_may(entity, arming, action);
        let probe = Kind::new(
            entity.to_string(),
            action.to_string(),
            None,
            Vec::new(),
            may,
            true,
        );
        self.kinds.iter().find(|kind| kind.same(&probe))
    }

    /// A reaction's target in the scope, its action, and the parameters it
    /// carries with their sorts.
    fn carried(&self, source: &str, trigger: &ActionTrigger) -> Option<Carried> {
        let (TriggerKind::Entity, Some(to), Some(action)) = (
            &trigger.kind,
            &trigger.target_entity,
            &trigger.target_action,
        ) else {
            return None;
        };
        if !self.scope.contains(to) {
            return None;
        }
        let target = self.action(to, action)?;
        let fields = trigger
            .args
            .iter()
            .map(|(param, arg)| {
                let sort = self
                    .param(target, param)
                    .or_else(|| match arg {
                        Arg::Var(var) => self.var(source, var),
                        Arg::Lit(Literal::Int(_)) => Some(Sort::Int { counter: false }),
                        Arg::Lit(Literal::Bool(_)) => Some(Sort::Bool),
                        _ => None,
                    })
                    .unwrap_or(Sort::Str);
                (param.clone(), sort)
            })
            .collect();
        Some((to.clone(), action.clone(), fields))
    }

    /// The kinds of dispatch the scope's records send each other, and the
    /// scheduled actions they arm.
    fn read_kinds(&self) -> Vec<Kind> {
        let mut kinds: Vec<Kind> = Vec::new();
        for source in &self.scope {
            for action in &self.automaton(source).actions {
                for trigger in &action.triggers {
                    let Some((to, target_action, fields)) = self.carried(source, trigger) else {
                        continue;
                    };
                    let may = self.reaction_may(source, &action.name, trigger, &to, &target_action);
                    let principal = trigger.principal.clone();
                    push_kind(
                        &mut kinds,
                        Kind::new(to, target_action, principal, fields, may, false),
                    );
                }
                for scheduled in action.effect.iter().filter_map(scheduled) {
                    if self.action(source, scheduled).is_some() {
                        let may = self.scheduled_may(source, &action.name, scheduled);
                        let (to, target) = (source.clone(), scheduled.to_string());
                        push_kind(
                            &mut kinds,
                            Kind::new(to, target, None, Vec::new(), may, true),
                        );
                    }
                }
            }
        }
        kinds
    }

    /// The module calls the scope's records start, by kind.
    fn read_calls(&self) -> Vec<Call> {
        let mut calls: Vec<Call> = Vec::new();
        for to in &self.scope {
            for action in &self.automaton(to).actions {
                for trigger in &action.triggers {
                    let Some(module) = trigger.callee() else {
                        continue;
                    };
                    let same = calls.iter().any(|c| {
                        &c.to == to
                            && c.module == module
                            && c.kind == trigger.kind
                            && c.on_success == trigger.on_success
                            && c.on_failure == trigger.on_failure
                    });
                    if same {
                        continue;
                    }
                    // The callee may be a template the kernel fills from a
                    // field, such as `{field:adapter_type}`, which a solver's
                    // name may not hold.
                    let base = format!("{to}@{}", symbol(module));
                    let mut name = base.clone();
                    let mut suffix = 2;
                    // A numbered name can also be another callee's base.
                    while calls.iter().any(|c| c.name == name) {
                        name = format!("{base}{suffix}");
                        suffix += 1;
                    }
                    calls.push(Call {
                        name,
                        to: to.clone(),
                        module: module.to_string(),
                        on_success: trigger.on_success.clone(),
                        on_failure: trigger.on_failure.clone(),
                        kind: trigger.kind,
                    });
                }
            }
        }
        calls
    }

    /// The call kind `trigger` on one of `entity`'s actions starts.
    pub(super) fn call_of(&self, entity: &str, trigger: &ActionTrigger) -> Option<&Call> {
        let module = trigger.callee()?;
        self.calls.iter().find(|c| {
            c.to == entity
                && c.module == module
                && c.kind == trigger.kind
                && c.on_success == trigger.on_success
                && c.on_failure == trigger.on_failure
        })
    }

    /// The actions the kernel takes as a call's answer: its declared
    /// answer, or else any action of its record (`call_may_answer`).
    pub(super) fn answer_actions(&self, call: &Call) -> Vec<&'a Action> {
        self.automaton(&call.to)
            .actions
            .iter()
            .filter(|a| call_may_answer(call.on_success.as_deref(), &a.name))
            .collect()
    }

    /// Everything that can be in flight, by kind: dispatches, then calls.
    pub(super) fn flights(&self) -> Vec<Flight<'_>> {
        let dispatches = self.kinds.iter().map(|k| Flight {
            name: &k.name,
            fields: k.fields.clone(),
        });
        let calls = self.calls.iter().map(|c| Flight {
            name: &c.name,
            fields: Vec::new(),
        });
        dispatches.chain(calls).collect()
    }
}

/// `text` with `_` for each character an SMT-LIB symbol may not hold.
fn symbol(text: &str) -> String {
    let allowed = |c: char| c.is_ascii_alphanumeric() || "~!@$%^&*_-+=<>.?/".contains(c);
    text.chars()
        .map(|c| if allowed(c) { c } else { '_' })
        .collect()
}

/// The action an effect schedules on its own record, if it schedules one.
pub(super) fn scheduled(effect: &Effect) -> Option<&str> {
    match effect {
        Effect::Schedule { action, .. } | Effect::ScheduleAt { action, .. } => Some(action),
        _ => None,
    }
}

/// Add `kind` unless `kinds` has it, named by its target and action (a
/// scheduled one `.scheduled` after) and numbered when several share it.
fn push_kind(kinds: &mut Vec<Kind>, mut kind: Kind) {
    if kinds.iter().any(|k| k.same(&kind)) {
        return;
    }
    let base = match kind.scheduled {
        true => format!("{}.{}.scheduled", kind.to, kind.action),
        false => format!("{}.{}", kind.to, kind.action),
    };
    let taken = kinds
        .iter()
        .filter(|k| k.to == kind.to && k.action == kind.action && k.scheduled == kind.scheduled)
        .count();
    kind.name = match taken {
        0 => base,
        n => format!("{base}{}", n + 1),
    };
    kinds.push(kind);
}
