//! What the joint model reads from the specs: the reactions between the
//! types in scope, and each type's model with the strings it must track.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};
use temper_spec::predicate::{Arg, Expr};

use super::super::env::Records;
use super::JOINT_MAX_COUNTER;
use crate::model::{TemperModel, build_model_tracking};

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
    pub(super) trigger: String,
    /// Whether the runtime may drop it without the drop check saying so: it
    /// creates its target (always ready for it), or `drop_ok` says a drop is
    /// meant.
    pub(super) may_drop: bool,
}

/// The models of the types in `scope` and the reactions between them,
/// tracking the string variables in `also`, the id fields references use
/// (when a reference finds a record by id), and what a reaction copies into
/// a tracked variable, until nothing more reaches one.
pub(super) fn tracked(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    mut also: BTreeMap<String, BTreeSet<String>>,
    records: Records,
) -> (BTreeMap<String, TemperModel>, Vec<Reaction>) {
    let reactions = reactions_in(by_name, scope);
    loop {
        let mut models: BTreeMap<String, TemperModel> = scope
            .iter()
            .map(|entity| {
                let extra = also.get(entity).cloned().unwrap_or_default();
                (
                    entity.clone(),
                    build_model_tracking(by_name[entity.as_str()], JOINT_MAX_COUNTER, &extra),
                )
            })
            .collect();
        let mut literals = BTreeSet::new();
        for entity in scope {
            let a = by_name[entity.as_str()];
            for inv in &a.invariants {
                collect_literals(&inv.assert, &mut literals);
            }
            for action in &a.actions {
                collect_literals(&action.guard, &mut literals);
            }
        }
        for model in models.values() {
            literals.extend(model.initial_strings.values().filter_map(|v| match v {
                crate::model::ModelString::Literal(s) => Some(s.clone()),
                _ => None,
            }));
        }
        for reaction in &reactions {
            for arg in reaction.args.values() {
                if let Arg::Lit(temper_spec::predicate::Literal::Str(s)) = arg {
                    literals.insert(s.clone());
                }
            }
        }
        for model in models.values_mut() {
            for field in &model.tracked_strings {
                model
                    .string_literals
                    .entry(field.clone())
                    .or_default()
                    .extend(literals.clone());
            }
        }
        let mut grew = false;
        let mut track = |entity: &str, field: &str| {
            !matches!(field, "Id" | "id")
                && also
                    .entry(entity.to_string())
                    .or_default()
                    .insert(field.to_string())
        };
        if records == Records::ById {
            for (entity, model) in &models {
                for t in &model.transitions {
                    for (related, id_field) in t.guard.related_types() {
                        if scope.contains(&related) {
                            grew |= track(entity, &id_field);
                        }
                    }
                }
            }
        }
        for reaction in &reactions {
            if records == Records::ById
                && let Some(TargetResolver::Field { id_field }) = &reaction.resolve
            {
                grew |= track(&reaction.from, id_field);
            }
            for (param, arg) in &reaction.args {
                if let Arg::Var(field) = arg
                    && models[&reaction.to].tracked_strings.contains(param)
                    && !models[&reaction.from].tracked_strings.contains(field)
                {
                    grew |= track(&reaction.from, field);
                }
            }
        }
        if !grew {
            return (models, reactions);
        }
    }
}

/// The entity reactions whose source and target are both in `scope`.
fn reactions_in(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> Vec<Reaction> {
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
                    trigger: trigger.name.clone(),
                    may_drop: trigger.drop_ok
                        || matches!(trigger.resolve_target, Some(TargetResolver::Create)),
                });
            }
        }
    }
    reactions
}

/// Literals must remain distinct even when compared with related operands.
fn collect_literals(expr: &Expr, out: &mut BTreeSet<String>) {
    use temper_spec::predicate::{Literal, Operand, Set};
    let mut operand = |o: &Operand| {
        if let Operand::Lit(Literal::Str(s)) = o {
            out.insert(s.clone());
        }
    };
    match expr {
        Expr::Compare { lhs, rhs, .. } => {
            operand(lhs);
            operand(rhs);
        }
        Expr::In {
            value,
            set: Set::List(items),
            ..
        } => {
            operand(value);
            for item in items {
                if let Literal::Str(s) = item {
                    out.insert(s.clone());
                }
            }
        }
        Expr::Not(e) => collect_literals(e, out),
        Expr::And(parts) | Expr::Or(parts) => {
            for e in parts {
                collect_literals(e, out);
            }
        }
        Expr::Implies(a, b) => {
            collect_literals(a, out);
            collect_literals(b, out);
        }
        _ => {}
    }
}
