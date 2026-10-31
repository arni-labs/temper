//! Rules on entering a place.
//!
//! An `[[entry_rule]]` must hold just before every move into its place from
//! a place it covers. A rule may read a related record's status, which one
//! record's model does not know, and the move's guard may read the same
//! status. Each such status is free, as it is for a guard alone, but it has
//! one value in the guard and the rule alike: a move breaks the rule when
//! some statuses of the related records it reads let its guard hold while
//! the rule is false. A guard that requires what the rule requires never
//! does.
//!
//! A related status is tried as each place the guard and the rule compare
//! it with, and as one place they name none of. A reference that lists
//! several records takes one status for all of them.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Env, Expr, Literal, Operand, Set, Truth, Val, eval};

use super::semantics::{ModelEnv, rule_for};
use super::types::{ResolvedEntryRule, ResolvedTransition, TemperModel, TemperModelState};

/// A related status: `(entity_type, id_field)`.
type Related = (String, String);

/// The status of a related record that is in none of the places the guard
/// and the rule name. A place's name has no spaces, so none equals it.
const ANOTHER_PLACE: &str = " another place ";

/// The spec's entry rules, spelling out the places each one covers.
pub(super) fn resolve_entry_rules(automaton: &Automaton) -> Vec<ResolvedEntryRule> {
    automaton
        .entry_rules
        .iter()
        .map(|rule| ResolvedEntryRule {
            name: rule.name.clone(),
            place: rule.place.clone(),
            from: if rule.from.is_empty() {
                let states = automaton.automaton.states.iter();
                states.filter(|s| **s != rule.place).cloned().collect()
            } else {
                rule.from.clone()
            },
            assert: rule.assert.clone(),
        })
        .collect()
}

/// The first move `state` enables that may break an entry rule, with the
/// rule.
pub(crate) fn entry_broken<'m>(
    model: &'m TemperModel,
    state: &TemperModelState,
) -> Option<(&'m ResolvedEntryRule, &'m ResolvedTransition)> {
    model
        .transitions
        .iter()
        .filter(|t| model.transition_enabled(t, state))
        .find_map(|t| broken_by(model, t, state).map(|rule| (rule, t)))
}

/// The first entry rule that taking `action` from `state` may break.
pub(crate) fn broken_by_action<'m>(
    model: &'m TemperModel,
    action: &str,
    state: &TemperModelState,
) -> Option<&'m ResolvedEntryRule> {
    broken_by(model, rule_for(model, action, &state.status)?, state)
}

/// The first entry rule that taking `t` from `state` may break.
fn broken_by<'m>(
    model: &'m TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> Option<&'m ResolvedEntryRule> {
    model.entry_rules.iter().find(|rule| {
        t.to_state.as_ref() == Some(&rule.place)
            && rule.from.contains(&state.status)
            && breaks(model, rule, t, state)
    })
}

/// Whether some statuses of the related records that `t`'s guard and
/// `rule` read let the guard hold in `state` while the rule is false.
fn breaks(
    model: &TemperModel,
    rule: &ResolvedEntryRule,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> bool {
    let related = related_statuses([&t.guard, &rule.assert]);
    any_assignment(&related, &mut BTreeMap::new(), &mut |statuses| {
        let env = Assumed {
            local: ModelEnv {
                kinds: &model.var_kinds,
                state,
            },
            statuses,
        };
        eval(&t.guard, &env).may_hold() && eval(&rule.assert, &env) == Truth::False
    })
}

/// Whether `test` passes for some status of each of `related`, after those
/// already in `assigned`.
fn any_assignment(
    related: &[(Related, Vec<String>)],
    assigned: &mut BTreeMap<Related, String>,
    test: &mut dyn FnMut(&BTreeMap<Related, String>) -> bool,
) -> bool {
    let Some(((key, places), rest)) = related.split_first() else {
        return test(assigned);
    };
    for place in places {
        assigned.insert(key.clone(), place.clone());
        if any_assignment(rest, assigned, test) {
            return true;
        }
    }
    false
}

/// Each related status `exprs` read, with every place they compare it with
/// and [`ANOTHER_PLACE`].
fn related_statuses(exprs: [&Expr; 2]) -> Vec<(Related, Vec<String>)> {
    let mut found: BTreeMap<Related, BTreeSet<String>> = BTreeMap::new();
    for expr in exprs {
        for related in expr.cross_refs() {
            found.entry(related).or_default();
        }
        compared_places(expr, &mut found);
    }
    found
        .into_iter()
        .map(|(related, places)| {
            let places = places.into_iter().chain([ANOTHER_PLACE.to_string()]);
            (related, places.collect())
        })
        .collect()
}

/// The places `expr` compares each related status with
/// (`T[id].status == 'x'`, `T[id].status in ['x', 'y']`).
fn compared_places(expr: &Expr, out: &mut BTreeMap<Related, BTreeSet<String>>) {
    fn note(operand: &Operand, place: &Literal, out: &mut BTreeMap<Related, BTreeSet<String>>) {
        if let (
            Operand::CrossStatus {
                entity_type,
                id_field,
            },
            Literal::Str(place),
        ) = (operand, place)
        {
            let related = (entity_type.clone(), id_field.clone());
            out.entry(related).or_default().insert(place.clone());
        }
    }
    match expr {
        Expr::Not(inner) => compared_places(inner, out),
        Expr::And(parts) | Expr::Or(parts) => {
            parts.iter().for_each(|part| compared_places(part, out));
        }
        Expr::Implies(a, b) => {
            compared_places(a, out);
            compared_places(b, out);
        }
        Expr::Compare { lhs, rhs, .. } => {
            for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                if let Operand::Lit(place) = other {
                    note(side, place, out);
                }
            }
        }
        Expr::In {
            value,
            set: Set::List(items),
            ..
        } => items.iter().for_each(|place| note(value, place, out)),
        _ => {}
    }
}

/// The record as the model knows it, with the related statuses assumed.
struct Assumed<'a> {
    local: ModelEnv<'a>,
    statuses: &'a BTreeMap<Related, String>,
}

impl Env for Assumed<'_> {
    fn status(&self) -> Val<'_> {
        self.local.status()
    }

    fn var(&self, name: &str) -> Val<'_> {
        self.local.var(name)
    }

    fn cross_statuses(&self, entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>> {
        let key = (entity_type.to_string(), id_field.to_string());
        self.statuses.get(&key).map(|place| vec![Val::Str(place)])
    }
}
