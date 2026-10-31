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
//! Related statuses cover concrete and symbolic equality classes from both
//! predicates and the local state. A reference list may contain different
//! statuses; the same assignment is used by the guard and the rule.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Env, Expr, Literal, Operand, Set, Truth, Val, VarKind, eval};

use super::semantics::{Expansion, ModelEnv, rule_for};
use super::types::{
    ModelString, ResolvedEntryRule, ResolvedTransition, TemperModel, TemperModelState,
};

/// A related status: `(entity_type, id_field)`.
type Related = (String, String);

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
    let exprs = [&t.guard, &rule.assert];
    let refs: BTreeSet<Related> = exprs.iter().flat_map(|expr| expr.cross_refs()).collect();
    let mut places = BTreeSet::from([ModelString::from(state.status.clone())]);
    // Nonliteral operands can name a concrete local string or an equality
    // symbol. Include both, so equality with either is a possible assignment.
    places.extend(state.strings.values().map(|value| match value {
        ModelString::BlankSymbol(n) => ModelString::Symbol(*n),
        other => other.clone(),
    }));
    places.extend(
        state
            .lists
            .values()
            .flatten()
            .cloned()
            .map(ModelString::from),
    );
    let mut comparisons = 0;
    for expr in exprs {
        compared_values(expr, &mut places, &mut comparisons);
    }
    // With no status-to-status comparison, all unnamed places are equivalent.
    // Otherwise each false universal comparison needs at most two witnesses;
    // keep one more per reference to keep every reference nonempty. Deleting
    // other records preserves all true universal comparisons. Fresh symbols
    // cover every equality relationship among those witnesses.
    let fresh = if comparisons == 0 {
        1
    } else {
        2 * comparisons + refs.len()
    };
    let first = places
        .iter()
        .filter_map(ModelString::symbol_id)
        .max()
        .unwrap_or(0)
        + 1;
    places.extend((first..first + fresh).map(ModelString::Symbol));
    let related = refs
        .into_iter()
        .map(|key| {
            let size = match model.var_kinds.get(&key.1) {
                Some(VarKind::Str) => 1,
                Some(VarKind::List) => state
                    .lists
                    .get(&key.1)
                    .map_or(1, |ids| ids.iter().collect::<BTreeSet<_>>().len().max(1)),
                // A reference outside the modeled state may be a list of any size.
                _ => places.len(),
            };
            (key, size)
        })
        .collect();
    StatusSearch {
        local: ModelEnv {
            kinds: &model.var_kinds,
            state,
        },
        guard: &t.guard,
        assertion: &rule.assert,
        related,
        places: places.into_iter().collect(),
        budget: Expansion::for_model(model),
    }
    .any_assignment(0, &mut BTreeMap::new())
}

/// A reference has one status per distinct record, shared by every read in
/// both predicates. Multiplicity and order do not affect universal predicates,
/// so enumerate nonempty subsets rather than permutations of the same values.
struct StatusSearch<'a> {
    local: ModelEnv<'a>,
    guard: &'a Expr,
    assertion: &'a Expr,
    related: Vec<(Related, usize)>,
    places: Vec<ModelString>,
    budget: Expansion,
}

impl StatusSearch<'_> {
    fn any_assignment(
        &self,
        index: usize,
        assigned: &mut BTreeMap<Related, Vec<ModelString>>,
    ) -> bool {
        if !self.budget.take() {
            return false;
        }
        if index == self.related.len() {
            let env = Assumed {
                local: &self.local,
                statuses: assigned,
            };
            return eval(self.guard, &env).may_hold() && eval(self.assertion, &env) == Truth::False;
        }
        self.choose(index, 0, &mut Vec::new(), assigned)
    }

    fn choose(
        &self,
        index: usize,
        start: usize,
        chosen: &mut Vec<ModelString>,
        assigned: &mut BTreeMap<Related, Vec<ModelString>>,
    ) -> bool {
        let (key, size) = &self.related[index];
        if !chosen.is_empty() {
            assigned.insert(key.clone(), chosen.clone());
            if self.any_assignment(index + 1, assigned) {
                return true;
            }
        }
        if chosen.len() < *size {
            for next in start..self.places.len() {
                if !self.budget.take() {
                    return false;
                }
                chosen.push(self.places[next].clone());
                if self.choose(index, next + 1, chosen, assigned) {
                    return true;
                }
                chosen.pop();
            }
        }
        false
    }
}

/// Concrete strings from either predicate, including literals on the other
/// side of a related-status comparison, and the comparisons that distinguish
/// different unnamed places from each other.
fn compared_values(expr: &Expr, out: &mut BTreeSet<ModelString>, comparisons: &mut usize) {
    fn note(operand: &Operand, out: &mut BTreeSet<ModelString>) {
        if let Operand::Lit(Literal::Str(value)) = operand {
            out.insert(value.clone().into());
        }
    }
    match expr {
        Expr::Not(inner) => compared_values(inner, out, comparisons),
        Expr::And(parts) | Expr::Or(parts) => {
            parts
                .iter()
                .for_each(|part| compared_values(part, out, comparisons));
        }
        Expr::Implies(a, b) => {
            compared_values(a, out, comparisons);
            compared_values(b, out, comparisons);
        }
        Expr::Compare { lhs, rhs, .. } => {
            note(lhs, out);
            note(rhs, out);
            if matches!(lhs, Operand::CrossStatus { .. })
                && matches!(rhs, Operand::CrossStatus { .. })
            {
                *comparisons += 1;
            }
        }
        Expr::In { value, set, .. } => {
            note(value, out);
            if let Set::List(items) = set {
                for item in items {
                    if let Literal::Str(value) = item {
                        out.insert(value.clone().into());
                    }
                }
            }
        }
        _ => {}
    }
}

/// The record as the model knows it, with the related statuses assumed.
struct Assumed<'a> {
    local: &'a ModelEnv<'a>,
    statuses: &'a BTreeMap<Related, Vec<ModelString>>,
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
        self.statuses
            .get(&key)
            .map(|places| places.iter().map(ModelString::val).collect())
    }
}
