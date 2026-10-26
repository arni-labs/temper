//! The check a new version of a spec passes before it replaces the installed
//! one, run by every install route before it writes or swaps anything.
//!
//! A new version may change anything, so long as no stored record is left
//! with no way forward: in a state the version drops, or in a state where
//! moves start and none is possible for the record's values though one was
//! under the installed version. The version says where such records go with
//! `[[carry]]` sections. Before anything is written, each carry is tried on
//! each record it applies to with the move code every move runs
//! ([`process_action`]): the carried record must hold to the new version's
//! invariants and have a way forward. The install then answers and the
//! version serves; the carries run after it in the background, as moves of
//! the platform journaled like any other ([`carry_pass`]).
//!
//! Records are read once per type, and only in the states a change can
//! affect: those the version drops, those a move with changed conditions
//! starts from ([`TransitionTable::move_conditions`]), and those a carry
//! starts from. One grouped catalog query covers each record whose catalog
//! row is current; each other record is read on its own.
//!
//! Reported, not refused: records the installed version already leaves with
//! no way forward, moves that are no longer possible for records that keep
//! another, and what is not checked: a move whose guard reads a related
//! record, which the stored record alone cannot answer, and a record that
//! cannot be read. An install that removes a whole type is not checked.

mod answer;
mod carry_pass;
mod records;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_jit::table::{Effect, Standing, TransitionTable};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{Automaton, Carry, parse_automaton};
use temper_spec::predicate::{Arg, Name};

use crate::entity_actor::{
    broken_invariant, build_eval_context_with_xref, process_action_with_xref,
};
use crate::state::ServerState;

pub use answer::{SpecChangeRefused, StuckRecords};
pub use carry_pass::CarryReport;

/// A new version of one entity type's spec, to be checked against the
/// version installed for it.
#[derive(Debug, Clone)]
pub struct SpecChange {
    /// The entity type.
    pub entity_type: String,
    /// The new version.
    pub proposed: Automaton,
}

/// One `[[carry]]` move on one record, made after the swap.
#[derive(Debug, Clone, PartialEq)]
pub struct CarryMove {
    /// The record's type.
    pub entity_type: String,
    /// The record.
    pub entity_id: String,
    /// The carry's move, `carry:<from>:<n>`.
    pub action: String,
    /// The values the carry sets.
    pub params: Value,
}

/// What a check found: the carries it tried, and what it reports.
#[derive(Debug, Clone, Default)]
pub struct SpecChangePlan {
    /// The carry moves, each tried already on the record as it is now.
    pub carries: Vec<CarryMove>,
    /// What the check found and did not refuse.
    pub warnings: Vec<String>,
}

/// The records of one type in one state that a carry applies to, in groups
/// that hold the same values of every name the check reads.
struct Carried<'a> {
    status: String,
    action: String,
    carry: &'a Carry,
    groups: Vec<Vec<String>>,
}

impl ServerState {
    /// The new versions among `sources`, each `(entity_type, IOA source)`:
    /// those of types `tenant` has installed whose source differs, parsed.
    pub fn spec_changes(
        &self,
        tenant: &TenantId,
        sources: &[(&str, &str)],
    ) -> Result<Vec<SpecChange>, String> {
        let registry = self.registry.read().expect("registry lock poisoned");
        let mut changes = Vec::new();
        for (entity_type, source) in sources {
            let Some(installed) = registry.get_spec(tenant, entity_type) else {
                continue;
            };
            if installed.ioa_source == *source {
                continue;
            }
            let proposed = parse_automaton(source)
                .map_err(|error| format!("spec {entity_type} failed to parse: {error}"))?;
            changes.push(SpecChange {
                entity_type: entity_type.to_string(),
                proposed,
            });
        }
        Ok(changes)
    }

    /// Check each new version against the version `tenant` has installed for
    /// its type and the records stored under it. A type with no installed
    /// version passes. Nothing is written.
    pub async fn check_spec_changes(
        &self,
        tenant: &TenantId,
        changes: &[SpecChange],
    ) -> Result<SpecChangePlan, SpecChangeRefused> {
        let mut plan = SpecChangePlan::default();
        let mut refused = SpecChangeRefused::default();
        for change in changes {
            let installed = self
                .registry
                .read()
                .expect("registry lock poisoned")
                .get_spec(tenant, &change.entity_type)
                .map(|spec| spec.table());
            let Some(old) = installed else {
                continue;
            };
            let new = TransitionTable::from_automaton(&change.proposed);
            self.check_type(tenant, change, (&old, &new), &mut plan, &mut refused)
                .await;
        }
        if refused.is_empty() {
            Ok(plan)
        } else {
            Err(refused)
        }
    }

    /// Check one type's new version, adding its carries and warnings to
    /// `plan` and what stops it to `refused`.
    async fn check_type(
        &self,
        tenant: &TenantId,
        change: &SpecChange,
        (old, new): (&TransitionTable, &TransitionTable),
        plan: &mut SpecChangePlan,
        refused: &mut SpecChangeRefused,
    ) {
        let entity_type = change.entity_type.as_str();
        // Whether a stored record's moves keep a new invariant depends on
        // their effects and the caller's parameters, which the check does
        // not have; each such move is checked when it is made.
        for invariant in &new.invariants {
            if !old
                .invariants
                .iter()
                .any(|before| before.name == invariant.name && before.assert == invariant.assert)
            {
                plan.warnings.push(format!(
                    "{entity_type}: invariant '{}' is new or changed. Stored records are not \
                     checked against it; a move that would break it is refused when it is made",
                    invariant.name
                ));
            }
        }
        let carries = change.proposed.carry_moves();
        let statuses = affected_states(old, new, &carries);
        if statuses.is_empty() {
            return;
        }
        let names = read_names(old, new, &carries);
        let every_state: BTreeSet<String> = old
            .states
            .iter()
            .chain(&new.states)
            .chain(&statuses)
            .cloned()
            .collect();
        let (groups, unreadable) = match self
            .record_groups(tenant, entity_type, &names, &statuses, &every_state)
            .await
        {
            Ok(found) => found,
            Err(error) => {
                refused.reasons.push(format!(
                    "{entity_type}: its stored records could not be read to check the new \
                     version: {error}"
                ));
                return;
            }
        };
        if unreadable > 0 {
            plan.warnings.push(format!(
                "{entity_type}: {unreadable} stored records could not be read, so the new \
                 version is not checked against them"
            ));
        }
        let mut carried: BTreeMap<String, Carried<'_>> = BTreeMap::new();
        let mut stuck: BTreeMap<String, (Standing, Vec<String>)> = BTreeMap::new();
        let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for group in groups {
            let before = old.stored_context(&group.values);
            let after = new.stored_context(&group.values);
            let was = old.standing(&group.status, &before);
            let now = new.standing(&group.status, &after);
            let newly_stuck = match (&was, &now) {
                (_, Standing::Free | Standing::Rests) => {
                    if let Some(note) = answer::lost_moves(
                        entity_type,
                        (old, new),
                        &group.status,
                        (&before, &after),
                    ) {
                        notes.entry(note).or_default().extend(group.ids);
                    }
                    continue;
                }
                (Standing::Gone, Standing::Gone) => false,
                (_, Standing::Gone) => true,
                (_, Standing::Stuck(blocked)) => {
                    if let Some(action) = blocked
                        .iter()
                        .map(|(action, _)| action)
                        .find(|action| reads_related(new, action))
                    {
                        let note = format!(
                            "{entity_type} in {}: {action}'s guard reads a related record, so \
                             whether these records can still move is not checked",
                            group.status
                        );
                        notes.entry(note).or_default().extend(group.ids);
                        continue;
                    }
                    was == Standing::Free
                }
            };
            let carry = carries.iter().find(|(action, carry)| {
                carry.from == group.status && new.blocked(&group.status, &after, action).is_none()
            });
            if let Some((action, carry)) = carry {
                carried
                    .entry(action.clone())
                    .or_insert_with(|| Carried {
                        status: group.status.clone(),
                        action: action.clone(),
                        carry,
                        groups: Vec::new(),
                    })
                    .groups
                    .push(group.ids);
            } else if newly_stuck {
                stuck
                    .entry(group.status.clone())
                    .or_insert_with(|| (now, Vec::new()))
                    .1
                    .extend(group.ids);
            } else {
                let note = format!(
                    "{entity_type} in {}: these records had no way forward under the \
                     installed version either, and no [[carry]] moves them",
                    group.status
                );
                notes.entry(note).or_default().extend(group.ids);
            }
        }
        for (note, ids) in notes {
            plan.warnings.push(answer::with_records(&note, &ids));
        }
        for (status, (now, mut ids)) in stuck {
            ids.sort();
            refused.add_stuck(entity_type, (old, new), &status, &now, ids);
        }
        for (_, carried) in carried {
            self.try_carry(tenant, entity_type, new, carried, plan, refused)
                .await;
        }
    }

    /// Try one carry on the records it applies to, as they are stored now,
    /// with the move code every move runs and the related records it reads,
    /// as a move resolves them. Each record it moves must hold to the new
    /// version's invariants and have a way forward.
    ///
    /// One record is tried for each group. The records of a group share
    /// their state and the values of every name the carry, the new version's
    /// moves and invariants, and its related records' references read
    /// ([`read_names`]), so the dry run ends the same way for each of them.
    /// A record is read whole from its journal, and a store read took 0.35
    /// to 0.8 s on a busy app drive (the Factory, 26 Oct 2026): one read
    /// per record would hold an install for minutes. A group whose first
    /// record cannot be read is tried with the next.
    async fn try_carry(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        new: &TransitionTable,
        carried: Carried<'_>,
        plan: &mut SpecChangePlan,
        refused: &mut SpecChangeRefused,
    ) {
        let params = serde_json::to_value(&carried.carry.set).unwrap_or_default();
        let mut refs = new.related_refs(&carried.action);
        for action in new.caller_moves() {
            for reference in new.related_refs(action) {
                if !refs.contains(&reference) {
                    refs.push(reference);
                }
            }
        }
        let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for group in carried.groups {
            let mut ids = group.into_iter();
            let mut tried = None;
            for id in ids.by_ref() {
                match self.current_record_state(tenant, entity_type, &id).await {
                    Some(record) => {
                        tried = Some((id, record));
                        break;
                    }
                    None => failures
                        .entry("it could not be read".to_string())
                        .or_default()
                        .push(id),
                }
            }
            let Some((id, mut record)) = tried else {
                continue;
            };
            let group: Vec<String> = std::iter::once(id).chain(ids).collect();
            let related = self
                .related_statuses_of(tenant, refs.clone(), &record.fields)
                .await;
            let moved =
                process_action_with_xref(&mut record, new, &carried.action, &params, &related);
            let why = if !moved.success {
                Some(format!(
                    "the carry is refused: {}",
                    moved.error.unwrap_or_default()
                ))
            } else if let Some(broken) = broken_invariant(&record, new) {
                Some(format!("once carried it would break {broken}"))
            } else {
                let ctx = build_eval_context_with_xref(&record, &related, new, None);
                match new.standing(&record.status, &ctx) {
                    Standing::Free | Standing::Rests => None,
                    Standing::Stuck(blocked) => Some(format!(
                        "once carried to {} it would still have no way forward: {}",
                        record.status,
                        answer::blocked_moves(&blocked)
                    )),
                    Standing::Gone => Some(format!(
                        "once carried it would be in {}, which is not a state of the new version",
                        record.status
                    )),
                }
            };
            match why {
                Some(why) => failures.entry(why).or_default().extend(group),
                None => plan
                    .carries
                    .extend(group.into_iter().map(|entity_id| CarryMove {
                        entity_type: entity_type.to_string(),
                        entity_id,
                        action: carried.action.clone(),
                        params: params.clone(),
                    })),
            }
        }
        for (why, ids) in failures {
            refused.reasons.push(answer::with_records(
                &format!(
                    "{entity_type} in {}: the [[carry]] from {} cannot carry these records: {why}",
                    carried.status, carried.carry.from
                ),
                &ids,
            ));
        }
    }
}

/// The states whose records a change can leave with no way forward, or a
/// carry moves: those `new` drops, those a move whose conditions changed
/// starts from under either version, and those a carry starts from.
fn affected_states(
    old: &TransitionTable,
    new: &TransitionTable,
    carries: &[(String, &Carry)],
) -> BTreeSet<String> {
    let mut states: BTreeSet<String> = old
        .states
        .iter()
        .filter(|state| !new.states.contains(state))
        .cloned()
        .collect();
    let moves: BTreeSet<&str> = old
        .caller_moves()
        .into_iter()
        .chain(new.caller_moves())
        .collect();
    for action in moves {
        if old.move_conditions(action) == new.move_conditions(action) {
            continue;
        }
        for rule in old.rules.iter().chain(&new.rules) {
            if rule.name != action {
                continue;
            }
            if rule.from_states.is_empty() {
                states.extend(old.states.iter().cloned());
            } else {
                states.extend(rule.from_states.iter().cloned());
            }
        }
    }
    states.extend(carries.iter().map(|(_, carry)| carry.from.clone()));
    states
}

/// The names the check reads from a record, which group the stored records:
/// what the moves of either version read, and under the new version what
/// each carry's guard and effects read, what its invariants read, and the
/// fields naming the related records its moves and invariants read. Records
/// in one state that agree on all of them end the same way in a carry's dry
/// run, so [`ServerState::try_carry`] reads one record per group.
fn read_names(
    old: &TransitionTable,
    new: &TransitionTable,
    carries: &[(String, &Carry)],
) -> Vec<String> {
    let mut names = BTreeSet::new();
    for table in [old, new] {
        for action in table.caller_moves() {
            names.extend(table.move_reads(action));
        }
    }
    let mut read = |name: Name<'_>| {
        if let Name::Var(var) = name {
            names.insert(var.to_string());
        }
    };
    for (_, carry) in carries {
        carry.guard.for_each_name(&mut read);
    }
    for invariant in &new.invariants {
        invariant.assert.for_each_name(&mut read);
    }
    for (action, _) in carries {
        names.extend(new.move_reads(action));
        let effects = new
            .rules
            .iter()
            .filter(|rule| rule.name == *action)
            .flat_map(|rule| &rule.effects);
        for effect in effects {
            let (var, value) = match effect {
                Effect::SetCounter { var, value }
                | Effect::AddCounter { var, value }
                | Effect::SubCounter { var, value }
                | Effect::SetBool { var, value }
                | Effect::ListAppend { var, value } => (var, value),
                Effect::ListRemoveAt { var, index } => (var, index),
                _ => continue,
            };
            names.insert(var.clone());
            if let Arg::Var(read) = value {
                names.insert(read.clone());
            }
        }
    }
    let actions = carries
        .iter()
        .map(|(action, _)| action.as_str())
        .chain(new.caller_moves());
    for action in actions {
        names.extend(new.related_refs(action).into_iter().map(|(_, field)| field));
    }
    names.into_iter().collect()
}

/// Whether a guard of `action` reads a related record.
fn reads_related(table: &TransitionTable, action: &str) -> bool {
    table
        .rules
        .iter()
        .any(|rule| rule.name == action && !rule.guard.related_types().is_empty())
}
