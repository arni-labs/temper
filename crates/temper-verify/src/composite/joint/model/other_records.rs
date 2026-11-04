//! What another record of a type can do to the records the walk holds.
//!
//! The model holds one record of each type, and the runtime holds any number.
//! Every read of another record is unknown ([`super::super::env`]), so a rule
//! over one says NOT CHECKED. Moves are the other side: a walk covers a move
//! that lands on a held record from a type outside the scope ([`super::from_outside`])
//! and from a request, but not from a second record of a type in scope.
//!
//! A reference can't be excused by renaming, by calling the record it names
//! the held one. Two holders may name one record, and then the second moves
//! that record while the first has not moved.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, TargetResolver, TriggerGraph};
use temper_spec::predicate::{Effect, Expr};

use super::Reaction;

#[derive(Debug, Default)]
pub(in crate::composite::joint) struct OtherRecords {
    /// The types whose reactions lead into the scope, through any other types.
    reach_scope: BTreeSet<String>,
    /// Why a second record may land on a held one, the walk not following it.
    lands: Option<String>,
}

impl OtherRecords {
    pub(super) fn read(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
        reactions: &[Reaction],
    ) -> Self {
        Self {
            reach_scope: reach(by_name, scope),
            lands: reaction_lands(reactions).or_else(|| spawn_lands(by_name, scope)),
        }
    }

    /// Whether another record of `entity` can set off a move on a record in
    /// scope. One that keeps to its own record stays away from the held one.
    pub(super) fn reach_scope(&self, entity: &str) -> bool {
        self.reach_scope.contains(entity)
    }

    /// Why no rule over this scope can say it holds, when one can't.
    pub(in crate::composite::joint) fn lands(&self) -> Option<&str> {
        self.lands.as_deref()
    }
}

/// Every type with a chain of reactions into `scope`, walking the reactions of
/// the whole app backwards.
fn reach(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> BTreeSet<String> {
    let automatons: Vec<&Automaton> = by_name.values().copied().collect();
    let graph = TriggerGraph::from_automatons(&automatons);
    let mut found = BTreeSet::new();
    let mut next: Vec<&str> = scope.iter().map(String::as_str).collect();
    while let Some(entity) = next.pop() {
        for edge in graph.incoming.get(entity).into_iter().flatten() {
            let own = edge.from == edge.to && edge.resolve_target == TargetResolver::SameId;
            if !own && found.insert(edge.from.clone()) {
                next.push(&edge.from);
            }
        }
    }
    found
}

/// The first reaction between types in scope that finds its target by a field:
/// any record of its source can name the held target.
fn reaction_lands(reactions: &[Reaction]) -> Option<String> {
    let reaction = reactions.iter().find(|reaction| {
        matches!(
            reaction.resolve,
            Some(TargetResolver::Field { .. } | TargetResolver::CreateIfMissing { .. })
        )
    })?;
    Some(format!(
        "another {} may send {}.{} to the {} the walk holds, and the walk holds one {}",
        reaction.from, reaction.to, reaction.target_action, reaction.to, reaction.from
    ))
}

/// The first spawn with an id of its own into a type in scope: any record of
/// the type that spawns it can name the held one.
fn spawn_lands(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> Option<String> {
    by_name.iter().find_map(|(from, automaton)| {
        let effects = automaton.actions.iter().flat_map(|action| &action.effect);
        effects.into_iter().find_map(|effect| match effect {
            Effect::Spawn {
                entity_type,
                initial_action,
                id: Some(_),
                ..
            } if scope.contains(entity_type) => Some(format!(
                "a {from} may spawn {entity_type}.{initial_action} on the {entity_type} the walk holds, and the walk does not follow spawns"
            )),
            _ => None,
        })
    })
}

/// Why `rule` of `owner` read unknown: naming its references, each of which may
/// name a record the model does not hold.
pub(in crate::composite::joint) fn unknown_read(owner: &str, rule: &Expr) -> String {
    let reads: BTreeSet<String> = rule
        .related_types()
        .into_iter()
        .map(|(_, field)| format!("{owner}.{field}"))
        .collect();
    if reads.is_empty() {
        return "it reads a value the walk does not follow".to_string();
    }
    format!(
        "it reads {}, which may name a record the walk does not hold",
        reads.into_iter().collect::<Vec<_>>().join(" and ")
    )
}
