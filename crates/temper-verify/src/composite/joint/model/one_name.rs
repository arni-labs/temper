//! Which references can read "no such record" when they name another id.
//!
//! The model holds one record of each type. A reference whose id field names
//! another id may find that record in the runtime, in any state, so it reads
//! as unknown. The exception is a type the scope names just one way, once:
//! any run of the record with the record it names can be renamed so that the
//! named record is the held one, and the walks start from every way ids can
//! be equal. Where the ids differ, all that is left to add is that no such
//! record exists.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, TargetResolver};
use temper_spec::predicate::{Arg, Effect, Expr};

use super::Reaction;

/// The references that read "no such record" when they name another id, by
/// the type that holds them: each with its id field and the type it names.
#[derive(Debug, Default)]
pub(in crate::composite::joint) struct OneNames(BTreeMap<String, BTreeSet<(String, String)>>);

impl OneNames {
    /// Whether `owner`'s `id_field` is the one name for `target` in scope.
    pub(in crate::composite::joint) fn has(
        &self,
        owner: &str,
        id_field: &str,
        target: &str,
    ) -> bool {
        self.0.get(owner).is_some_and(|names| {
            names
                .iter()
                .any(|(field, to)| field == id_field_name(id_field) && to == target)
        })
    }

    /// A reference qualifies when all of these hold:
    /// - it is the only one the scope has to its type, counting what reads
    ///   the type, what a reaction lands on and where a spawn stores its id,
    ///   and it is held by another type: with two names, or a name on the
    ///   type's own records, one record cannot be both;
    /// - its id field is written only by moves that leave the initial state
    ///   for good: moved later, it names a second record;
    /// - the type it names cannot act back on its holder: a reaction would
    ///   land on some other holder, which the model does not hold.
    pub(super) fn read(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
        reactions: &[Reaction],
    ) -> Self {
        let mut names = Self::default();
        for (target, holders) in references(by_name, scope, reactions) {
            let mut holders = holders.into_iter();
            let (Some((owner, field)), None) = (holders.next(), holders.next()) else {
                continue;
            };
            if owner != target
                && written_once(by_name[owner.as_str()], &field)
                && !acts_back(by_name[target.as_str()], &target, &owner, reactions)
            {
                names.0.entry(owner).or_default().insert((field, target));
            }
        }
        names
    }
}

/// `Id` and `id` both read a record's own id.
fn id_field_name(field: &str) -> &str {
    if matches!(field, "Id" | "id") {
        "Id"
    } else {
        field
    }
}

/// For each type in scope, every (type, id field) that names one of its
/// records.
fn references(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    reactions: &[Reaction],
) -> BTreeMap<String, BTreeSet<(String, String)>> {
    let mut found: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut add = |target: &str, owner: &str, field: &str| {
        if scope.contains(target) {
            found
                .entry(target.to_string())
                .or_default()
                .insert((owner.to_string(), id_field_name(field).to_string()));
        }
    };
    for owner in scope {
        let automaton = by_name[owner.as_str()];
        for (target, field) in rule_reads(automaton) {
            add(&target, owner, &field);
        }
        for effect in automaton.actions.iter().flat_map(|action| &action.effect) {
            if let Effect::Spawn {
                entity_type,
                store_id_in: Some(field),
                ..
            } = effect
            {
                add(entity_type, owner, field);
            }
        }
    }
    for reaction in reactions {
        if let Some(guard) = &reaction.guard {
            for (target, field) in guard.related_types() {
                add(&target, &reaction.from, &field);
            }
        }
        match &reaction.resolve {
            Some(TargetResolver::SameId) => add(&reaction.to, &reaction.from, "Id"),
            Some(TargetResolver::Field { id_field })
            | Some(TargetResolver::CreateIfMissing { id_field }) => {
                add(&reaction.to, &reaction.from, id_field);
            }
            _ => {}
        }
    }
    found
}

/// The (type, id field) each guard and rule of `automaton` reads.
fn rule_reads(automaton: &Automaton) -> Vec<(String, String)> {
    let guards = automaton.actions.iter().map(|action| &action.guard);
    let invariants = automaton.invariants.iter().map(|rule| &rule.assert);
    let entries = automaton.entry_rules.iter().map(|rule| &rule.assert);
    let goals = automaton
        .liveness
        .iter()
        .filter_map(|rule| rule.eventually.as_ref());
    guards
        .chain(invariants)
        .chain(entries)
        .chain(goals)
        .flat_map(Expr::related_types)
        .collect()
}

/// Whether only moves that leave `automaton`'s initial state for good write
/// `field`. A move from there back to there can repeat.
fn written_once(automaton: &Automaton, field: &str) -> bool {
    if field == "Id" {
        return true;
    }
    let initial = &automaton.automaton.initial;
    automaton.actions.iter().all(|action| {
        let leaves = !action.from.is_empty()
            && action.from.iter().all(|from| from == initial)
            && action.to.as_ref().is_some_and(|to| to != initial);
        leaves || !writes(action, field)
    }) && !automaton.actions.iter().any(|action| {
        action.to.as_ref() == Some(initial) && action.from.iter().any(|from| from != initial)
    })
}

fn writes(action: &temper_spec::automaton::Action, field: &str) -> bool {
    let names = |arg: &Arg| matches!(arg, Arg::Var(name) | Arg::Param(name) if name == field);
    action.params.iter().any(|param| param.name() == field)
        || action.effect.iter().any(|effect| match effect {
            Effect::Assign { var, .. } => var == field,
            Effect::Spawn {
                store_id_in, id, ..
            } => store_id_in.as_deref() == Some(field) || id.as_ref().is_some_and(names),
            _ => false,
        })
}

/// Whether `target` reacts on `owner` or reads it in a guard.
fn acts_back(target_spec: &Automaton, target: &str, owner: &str, reactions: &[Reaction]) -> bool {
    reactions.iter().any(|r| r.from == target && r.to == owner)
        || rule_reads(target_spec)
            .iter()
            .any(|(related, _)| related == owner)
}
