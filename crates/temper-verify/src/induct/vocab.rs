//! What an inductive proof of a rule speaks of: the records of the types the
//! rule names, one per type with the same id, the dispatches in flight
//! between them by kind, and what Cedar lets each sender send.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_authz::{Asker, Attr, May, SendAnalysis};
use temper_spec::automaton::{
    Action, ActionKind, ActionParam, ActionTrigger, Automaton, TargetResolver, TriggerKind, VarType,
};
use temper_spec::predicate::{Arg, Expr, Literal, Operand, Set};

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
/// parameters, run as the same sender and that Cedar answers the same.
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
}

/// The vocabulary of one rule's proof.
pub(super) struct Vocab<'a> {
    pub(super) by_name: BTreeMap<&'a str, &'a Automaton>,
    pub(super) scope: BTreeSet<String>,
    /// Each record's variables the proof holds, with their sorts.
    pub(super) vars: BTreeMap<String, BTreeMap<String, Sort>>,
    pub(super) kinds: Vec<Kind>,
    /// Every string literal the specs, the rule and its lemmas name.
    pub(super) literals: BTreeSet<String>,
    policies: Option<&'a SendAnalysis>,
}

impl<'a> Vocab<'a> {
    pub(super) fn new(
        by_name: BTreeMap<&'a str, &'a Automaton>,
        scope: BTreeSet<String>,
        policies: Option<&'a SendAnalysis>,
        lemma_literals: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut vars = BTreeMap::new();
        let mut literals: BTreeSet<String> = lemma_literals.into_iter().collect();
        literals.insert(String::new());
        for entity in &scope {
            let automaton = by_name[entity.as_str()];
            let held: BTreeMap<String, Sort> = automaton
                .state
                .iter()
                .filter_map(|var| Sort::of(&var.var_type).map(|sort| (var.name.clone(), sort)))
                .collect();
            vars.insert(entity.clone(), held);
            collect_literals(automaton, &mut literals);
        }
        let mut vocab = Self {
            by_name,
            scope,
            vars,
            kinds: Vec::new(),
            literals,
            policies,
        };
        vocab.kinds = vocab.read_kinds();
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
        self.kinds.iter().find(|kind| {
            kind.to == to
                && kind.action == target_action
                && kind.principal == trigger.principal
                && kind.may == may
                && kind
                    .fields
                    .iter()
                    .map(|(f, _)| f)
                    .eq(fields.iter().map(|(f, _)| f))
        })
    }

    /// Whether Cedar lets someone outside send `action` to `entity`.
    pub(super) fn outside_may(&self, entity: &str, action: &str) -> May {
        self.policies.map_or(May::Yes, |policies| {
            policies.may(&Asker::Outside, action, entity, &BTreeMap::new())
        })
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
            Some(asker) => policies.may(&asker, action, to, &BTreeMap::new()),
            None => May::Maybe,
        }
    }

    /// A reaction's target in the scope, its action, and the parameters it
    /// carries with their sorts.
    fn carried(
        &self,
        source: &str,
        trigger: &ActionTrigger,
    ) -> Option<(String, String, Vec<(String, Sort)>)> {
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

    /// The kinds of dispatch the scope's records send each other.
    fn read_kinds(&self) -> Vec<Kind> {
        let mut kinds: Vec<Kind> = Vec::new();
        for source in &self.scope {
            for action in &self.automaton(source).actions {
                for trigger in &action.triggers {
                    let Some((to, target_action, fields)) = self.carried(source, trigger) else {
                        continue;
                    };
                    let may = self.reaction_may(source, &action.name, trigger, &to, &target_action);
                    let same = kinds.iter().any(|kind| {
                        kind.to == to
                            && kind.action == target_action
                            && kind.principal == trigger.principal
                            && kind.may == may
                            && kind
                                .fields
                                .iter()
                                .map(|(f, _)| f)
                                .eq(fields.iter().map(|(f, _)| f))
                    });
                    if same {
                        continue;
                    }
                    let taken = kinds
                        .iter()
                        .filter(|k| k.to == to && k.action == target_action)
                        .count();
                    let name = match taken {
                        0 => format!("{to}.{target_action}"),
                        n => format!("{to}.{target_action}{}", n + 1),
                    };
                    kinds.push(Kind {
                        name,
                        to,
                        action: target_action,
                        principal: trigger.principal.clone(),
                        fields,
                        may,
                    });
                }
            }
        }
        kinds
    }

    /// The actions a module call's answer may take on `entity`: its internal
    /// actions, and the declared answers and failures of its triggers.
    pub(super) fn answers(&self, entity: &str) -> BTreeSet<String> {
        let automaton = self.automaton(entity);
        let mut answers: BTreeSet<String> = automaton
            .actions
            .iter()
            .filter(|a| a.kind == ActionKind::Internal)
            .map(|a| a.name.clone())
            .collect();
        for action in &automaton.actions {
            for trigger in action
                .triggers
                .iter()
                .filter(|t| t.kind == TriggerKind::Wasm)
            {
                answers.extend(trigger.on_success.iter().cloned());
                answers.extend(trigger.on_failure.iter().cloned());
            }
        }
        answers
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

fn collect_literals(automaton: &Automaton, literals: &mut BTreeSet<String>) {
    let mut lit = |l: &Literal| {
        if let Literal::Str(s) = l {
            literals.insert(s.clone());
        }
    };
    let expr_lits = |e: &Expr, lit: &mut dyn FnMut(&Literal)| walk_expr(e, lit);
    for action in &automaton.actions {
        expr_lits(&action.guard, &mut lit);
        for effect in &action.effect {
            if let temper_spec::predicate::Effect::Assign {
                value: Arg::Lit(l), ..
            } = effect
            {
                lit(l);
            }
        }
        for trigger in &action.triggers {
            for arg in trigger.args.values() {
                if let Arg::Lit(l) = arg {
                    lit(l);
                }
            }
        }
    }
    for invariant in &automaton.invariants {
        expr_lits(&invariant.assert, &mut lit);
    }
    for var in &automaton.state {
        if let temper_spec::automaton::Initial::String(s) = &var.initial {
            lit(&Literal::Str(s.clone()));
        }
    }
}

fn walk_expr(expr: &Expr, lit: &mut dyn FnMut(&Literal)) {
    match expr {
        Expr::Not(e) => walk_expr(e, lit),
        Expr::And(es) | Expr::Or(es) => es.iter().for_each(|e| walk_expr(e, lit)),
        Expr::Implies(a, b) => {
            walk_expr(a, lit);
            walk_expr(b, lit);
        }
        Expr::Compare { lhs, rhs, .. } => {
            for side in [lhs, rhs] {
                if let Operand::Lit(l) = side {
                    lit(l);
                }
            }
        }
        Expr::In { value, set, .. } => {
            if let Operand::Lit(l) = value {
                lit(l);
            }
            if let Set::List(items) = set {
                items.iter().for_each(|l| lit(l));
            }
        }
        Expr::Const(_) | Expr::Empty(_) | Expr::Var(_) => {}
    }
}
