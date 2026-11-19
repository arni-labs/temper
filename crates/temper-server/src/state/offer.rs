//! Whether a record could make a move now, for a page that offers moves
//! before any is sent (`POST /api/authorize`).
//!
//! Both answers come from the check every move goes through
//! ([`TransitionTable::blocked`]) and say a refusal as a dispatch of the
//! move says it ([`move_refusal`]), so an offer and the move it offers
//! cannot disagree about a state or a guard.

use std::collections::BTreeMap;

use serde_json::Value;
use temper_authz::SecurityContext;
use temper_jit::table::{Blocked, EvalContext, Refusal, TransitionRule, TransitionTable};
use temper_runtime::tenant::TenantId;

use super::ServerState;
use crate::entity_actor::{build_eval_context_with_xref, move_refusal};

/// What the snapshot a caller sent says of one move it asked about.
pub(crate) enum OnSnapshot {
    /// The state and the guard let it be made, or it is no move of the spec:
    /// Cedar answers on the snapshot.
    Free,
    /// The state or the guard rules it out: the message and typed refusal a
    /// dispatch of it gets.
    Refused(String, Refusal),
    /// Its guard reads another record. The snapshot cannot say which record:
    /// the stored one decides ([`ServerState::offers_on_stored`]).
    ReadsRelated,
}

/// An answer about one move: why not, when not, with the typed refusal when
/// the record's state or guard is why.
pub(crate) type OfferAnswer = (Option<String>, Option<Refusal>);

impl ServerState {
    /// What `snapshot`, a record of `entity_type` as its caller read it, says
    /// of each of `actions`. Nothing is read.
    ///
    /// A value `snapshot` lacks reads as its declared initial value. A guard
    /// that reads the caller's parameters is decided by its parts that read
    /// none, so a move some call could make is free.
    ///
    /// Every move is free when `entity_type` has no spec in `tenant` or
    /// `snapshot` has no `status`: there is no record state to judge, and
    /// Cedar alone answers, as it does for an action the spec has no rule for.
    pub(crate) fn offers_on_snapshot(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        snapshot: &BTreeMap<String, Value>,
        actions: &[String],
    ) -> Vec<OnSnapshot> {
        let table = self
            .registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_table(tenant, entity_type);
        let status = snapshot.get("status").and_then(Value::as_str);
        let (Some(table), Some(status)) = (table, status) else {
            return actions.iter().map(|_| OnSnapshot::Free).collect();
        };
        let ctx = table.stored_context(snapshot);
        actions
            .iter()
            .map(|action| {
                let reads_related = table.rules.iter().any(|rule| {
                    rule.name == *action
                        && starts_from(rule, status)
                        && !rule.guard.cross_refs().is_empty()
                });
                if reads_related {
                    return OnSnapshot::ReadsRelated;
                }
                match refusal(&table, status, &ctx, action) {
                    Some((message, refusal)) => OnSnapshot::Refused(message, refusal),
                    None => OnSnapshot::Free,
                }
            })
            .collect()
    }

    /// Whether `security_ctx` could make each of `actions` on the stored
    /// record `entity_id`, decided as a dispatch decides it: the record is
    /// read, Cedar is asked on its stored attributes, and a move Cedar lets
    /// through is judged on the record's state and its guard, with the
    /// statuses of the related records the guard reads resolved from the
    /// record's own references.
    ///
    /// A record or spec that cannot be read answers every move with why, so
    /// nothing is offered on a guess.
    ///
    /// Cost: one read of the record, and one status lookup for each related
    /// record the guards of `actions` read, from the status cache when it
    /// holds it. Nothing is written.
    pub(crate) async fn offers_on_stored(
        &self,
        tenant: &TenantId,
        security_ctx: &SecurityContext,
        entity_type: &str,
        entity_id: &str,
        actions: &[String],
    ) -> Vec<OfferAnswer> {
        let unread = |why: String| -> Vec<OfferAnswer> {
            let why = format!("{entity_type} '{entity_id}' could not be read: {why}");
            actions.iter().map(|_| (Some(why.clone()), None)).collect()
        };
        let stored = match self
            .load_authz_resource_snapshot(tenant, entity_type, entity_id)
            .await
        {
            Ok(stored) => stored,
            Err(error) => return unread(error.to_string()),
        };
        let table = match self.transition_table_for_dispatch(tenant, entity_type) {
            Ok(table) => table,
            Err(error) => return unread(error.to_string()),
        };
        let record = &stored.current_state.state;
        let mut refs: Vec<(String, String)> = Vec::new();
        for rule in &table.rules {
            if !actions.contains(&rule.name) || !starts_from(rule, &record.status) {
                continue;
            }
            for reference in rule.guard.cross_refs() {
                if !refs.contains(&reference) {
                    refs.push(reference);
                }
            }
        }
        let related = self
            .related_statuses_of(tenant, &table, refs, &record.fields)
            .await;
        let ctx = build_eval_context_with_xref(record, &related, &table, None);
        actions
            .iter()
            .map(|action| {
                let asked = self.authorize_offer_with_context(
                    security_ctx,
                    action,
                    entity_type,
                    &stored.resource_attrs,
                    tenant.as_str(),
                );
                if let Err(denial) = asked {
                    return (Some(denial.to_string()), None);
                }
                match refusal(&table, &record.status, &ctx, action) {
                    Some((message, refusal)) => (Some(message), Some(refusal)),
                    None => (None, None),
                }
            })
            .collect()
    }
}

/// Whether `rule` starts from `status`: a rule that names no state starts
/// from any.
fn starts_from(rule: &TransitionRule, status: &str) -> bool {
    rule.from_states.is_empty() || rule.from_states.iter().any(|from| from == status)
}

/// Why a record in `status` with the values in `ctx` cannot make `action`,
/// as a dispatch of it would be refused, or `None` when it can or `action`
/// is no move of the spec.
fn refusal(
    table: &TransitionTable,
    status: &str,
    ctx: &EvalContext,
    action: &str,
) -> Option<(String, Refusal)> {
    match table.blocked(status, ctx, action)? {
        Blocked::NoAction => None,
        Blocked::NotFrom => Some(move_refusal(table, action, status, None)),
        Blocked::Guard(failure) => Some(move_refusal(table, action, status, failure)),
    }
}
