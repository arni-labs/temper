//! The rules over one record that the kernel checks on every write: its
//! `[[invariant]]`s and `[[field_invariant]]`s.
//!
//! The verification cascade proves a spec's invariants over its model; a
//! field invariant reads values the model does not track. Either way a record
//! can reach a state the model never reached: it was written before a rule
//! existed, under a spec that was never verified, or with values the model
//! does not track. So every write is checked, whoever makes it: a person, a
//! reaction, a timer, a module's answer or a carry. Both kinds are the same
//! rule at run time, one record's own values, so they are one list here and
//! one check. A write that would make a rule false, when it held before the
//! write, is refused. A rule that did not hold before the write either is not
//! the write's doing, and does not stop it: the record is not stranded by
//! it, and a write that repairs it goes through.
//!
//! A move resolves the statuses of the records it points at, never their
//! fields, so an invariant that reads another record's field is not checked
//! on moves (see [`MoveInvariant::from_spec`]).

use serde::{Deserialize, Serialize};
use temper_spec::automaton::{FieldInvariant, Invariant};
use temper_spec::predicate::{Expr, Name};

use super::guard::{self, EvalContext, GuardFailure};
use super::types::TransitionTable;

/// An `[[invariant]]` or `[[field_invariant]]` the kernel checks on every
/// write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveInvariant {
    /// The invariant's name, as the spec writes it.
    pub name: String,
    /// What must hold.
    pub assert: Expr,
    /// Which section of the spec declares it.
    #[serde(default)]
    pub declared_as: DeclaredAs,
    /// What the spec says when it does not hold (a field invariant's
    /// `message`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The section of a spec a [`MoveInvariant`] comes from. Both are checked
/// the same way; a write that breaks a field invariant is answered as a
/// broken data rule (`ConstraintViolation`), as it always was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredAs {
    /// An `[[invariant]]`, which the verification cascade proves too.
    #[default]
    Invariant,
    /// A `[[field_invariant]]`, over values the cascade does not model.
    FieldInvariant,
}

impl MoveInvariant {
    /// Whether moves check `invariant`. They cannot when it reads another
    /// record's field.
    pub fn checked(invariant: &Invariant) -> bool {
        !invariant.assert.reads_related_field()
    }

    /// The invariant as moves check it, or `None` when they cannot.
    pub fn from_spec(invariant: &Invariant) -> Option<Self> {
        Self::checked(invariant).then(|| Self {
            name: invariant.name.clone(),
            assert: invariant.assert.clone(),
            declared_as: DeclaredAs::Invariant,
            message: None,
        })
    }

    /// The field invariant as writes check it, or `None` when they cannot
    /// (it reads another record's field).
    pub fn from_field_invariant(invariant: &FieldInvariant) -> Option<Self> {
        (!invariant.assert.reads_related_field()).then(|| Self {
            name: invariant.name.clone(),
            assert: invariant.assert.clone(),
            declared_as: DeclaredAs::FieldInvariant,
            message: invariant.message.clone(),
        })
    }

    /// Whether a move changed which records this invariant reads the status
    /// of. The related statuses were read for the records the move started
    /// with, so they say nothing about the new ones.
    fn repointed(&self, before: &EvalContext, after: &EvalContext) -> bool {
        self.assert.cross_refs().iter().any(|(_, id_field)| {
            before.fields.get(id_field) != after.fields.get(id_field)
                || before.lists.get(id_field) != after.lists.get(id_field)
        })
    }
}

/// What a move does to the type's invariants.
#[derive(Debug, Default)]
pub struct InvariantsAcrossMove<'t> {
    /// The first invariant the move would break: it held before the move and
    /// does not after it.
    pub broken: Option<(&'t MoveInvariant, GuardFailure)>,
    /// The invariants that did not hold before the move: the record already
    /// breaks them.
    pub already_broken: Vec<(&'t MoveInvariant, GuardFailure)>,
}

impl TransitionTable {
    /// Compare the type's invariants before a move and after it. `before`
    /// and `after` are the status and the values the invariants read, before
    /// and after the move.
    pub fn invariants_across_move(
        &self,
        before: (&str, &EvalContext),
        after: (&str, &EvalContext),
    ) -> InvariantsAcrossMove<'_> {
        let mut result = InvariantsAcrossMove::default();
        for invariant in &self.invariants {
            if let Some(failure) =
                guard::check_detailed(&invariant.assert, before.0, before.1, &self.initial_values)
            {
                result.already_broken.push((invariant, failure));
                continue;
            }
            if invariant.repointed(before.1, after.1) {
                continue;
            }
            if let Some(failure) =
                guard::check_detailed(&invariant.assert, after.0, after.1, &self.initial_values)
            {
                result.broken = Some((invariant, failure));
                break;
            }
        }
        result
    }

    /// The first invariant that is false for a record in `status` with the
    /// values in `ctx`, with its failing part. Those that read another
    /// record's status are left out: the record alone cannot answer them.
    pub fn broken_invariant(
        &self,
        status: &str,
        ctx: &EvalContext,
    ) -> Option<(&MoveInvariant, GuardFailure)> {
        self.invariants
            .iter()
            .filter(|invariant| invariant.assert.cross_refs().is_empty())
            .find_map(|invariant| {
                guard::check_detailed(&invariant.assert, status, ctx, &self.initial_values)
                    .map(|failure| (invariant, failure))
            })
    }

    /// The first invariant this version adds or changes from `installed`
    /// that a stored record in `status` breaks, and did not break under
    /// `installed`: the record would be one its new spec forbids. `now` and
    /// `before` are its values as each version reads them. Those that read
    /// another record are left out: the record alone cannot answer them.
    pub fn newly_broken_invariant(
        &self,
        installed: &TransitionTable,
        status: &str,
        (now, before): (&EvalContext, &EvalContext),
    ) -> Option<(&MoveInvariant, GuardFailure)> {
        self.invariants
            .iter()
            .filter(|invariant| invariant.assert.cross_refs().is_empty())
            .find_map(|invariant| {
                if !self.changes_invariant(installed, invariant) {
                    return None;
                }
                let was = installed
                    .invariants
                    .iter()
                    .find(|old| old.name == invariant.name);
                let failure =
                    guard::check_detailed(&invariant.assert, status, now, &self.initial_values)?;
                let held = was.is_none_or(|old| {
                    guard::check_detailed(&old.assert, status, before, &installed.initial_values)
                        .is_none()
                });
                held.then_some((invariant, failure))
            })
    }

    /// The names this version's invariants read.
    pub fn invariant_names(&self) -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        for invariant in &self.invariants {
            invariant.assert.for_each_name(&mut |name| {
                if let Name::Var(name) = name {
                    names.insert(name.to_string());
                }
            });
        }
        names
    }

    /// Whether this version adds or changes an invariant from `installed`
    /// that a stored record alone can answer.
    pub fn adds_record_invariant(&self, installed: &TransitionTable) -> bool {
        self.invariants.iter().any(|invariant| {
            invariant.assert.cross_refs().is_empty() && self.changes_invariant(installed, invariant)
        })
    }

    /// Whether this version's `invariant` is new or changed from
    /// `installed`: its name is new, its assertion differs, or a name it
    /// reads is declared another way (its kind or initial value), which
    /// changes what a record that never stored that name reads.
    pub fn changes_invariant(
        &self,
        installed: &TransitionTable,
        invariant: &MoveInvariant,
    ) -> bool {
        let Some(old) = installed
            .invariants
            .iter()
            .find(|old| old.name == invariant.name)
        else {
            return true;
        };
        let mut changed = old.assert != invariant.assert;
        invariant.assert.for_each_name(&mut |name| {
            if let Name::Var(name) = name {
                changed |= self.declared(name) != installed.declared(name);
            }
        });
        changed
    }
}

#[cfg(test)]
#[path = "invariant_test.rs"]
mod tests;
