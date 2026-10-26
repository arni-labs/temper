//! A record's transition history, read page by page from the durable journal.
//!
//! Both history routes — the record's OData `$history` and the operator's
//! observe history — answer through [`read_history_page`]. Each route keeps
//! its own authorization; this module only reads and shapes the journal.
//!
//! The journal is the source because the actor's in-memory event list only
//! holds the events since the last snapshot. Each event carries its stored
//! sequence number, so `after=<sequence>` paging stays stable while new moves
//! are appended.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use temper_runtime::persistence::{COMPOSITE_EVENT_TYPE, PersistenceEnvelope, PersistenceError};
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::entity_actor::{CompositeAudit, EntityEvent};
use crate::odata::read_page_budget;
use crate::state::ServerState;

/// One page of history: events stored after `after`, at most `limit` of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HistoryPageRequest {
    /// Stored sequence number the page starts after (0 = from the start).
    pub(crate) after: u64,
    /// Number of journal entries the page may cover.
    pub(crate) limit: usize,
    /// The `limit` the caller asked for, before the page budget applied.
    pub(crate) requested_limit: usize,
}

impl HistoryPageRequest {
    /// Parse the `after` and `limit` query options.
    ///
    /// `limit` defaults to, and is clamped to, the OData collection page
    /// budget: a page is built in memory as one response, the same as any
    /// collection read. A clamp is reported on the span, never asserted.
    pub(crate) fn from_query(params: &BTreeMap<String, String>) -> Result<Self, String> {
        let after = match params.get("after") {
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("after must be a stored sequence number, got '{raw}'"))?,
            None => 0,
        };
        let (default_limit, max_limit) = read_page_budget();
        let limit = match params.get("limit") {
            Some(raw) => match raw.trim().parse::<usize>() {
                Ok(0) | Err(_) => {
                    return Err(format!("limit must be a positive integer, got '{raw}'"));
                }
                Ok(limit) => limit,
            },
            None => default_limit,
        };
        Ok(Self {
            after,
            limit: limit.min(max_limit),
            requested_limit: limit,
        })
    }
}

/// Why a history page could not be read.
#[derive(Debug)]
pub(crate) enum HistoryReadError {
    /// The server runs without a durable journal, so there is no history.
    NoJournal,
    /// The journal read failed.
    Storage(PersistenceError),
}

impl HistoryReadError {
    /// The HTTP status and OData error code this failure answers with.
    pub(crate) fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::NoJournal => (StatusCode::NOT_IMPLEMENTED, "HistoryUnavailable"),
            Self::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "HistoryReadError"),
        }
    }

    /// A message for the caller.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::NoJournal => "this server has no durable event journal".to_string(),
            Self::Storage(error) => format!("history read failed: {error}"),
        }
    }
}

/// Read one page of an entity's transitions from the durable journal.
///
/// Returns `{entity_type, entity_id, events: [...], next}`, where `next` is
/// the stored sequence to pass as `after` for the following page, or null on
/// the last page. A composite parent's audit record appears as the composite
/// action, with the sub-writes it committed in place of a state change.
/// An undecodable entry fails the page rather than advancing past missing history.
///
/// The caller must already have authorized the read.
#[instrument(skip_all, fields(
    otel.name = "entity.history_page",
    tenant = %tenant,
    entity_type = %entity_type,
    history.after = request.after,
    history.limit = request.limit,
    history.limit_requested = request.requested_limit,
    history.returned = tracing::field::Empty,
    history.has_next = tracing::field::Empty,
))]
pub(crate) async fn read_history_page(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    request: HistoryPageRequest,
) -> Result<serde_json::Value, HistoryReadError> {
    debug_assert!(request.limit > 0, "history page limit must be positive");
    let (store, _backend) = state.event_journal().ok_or(HistoryReadError::NoJournal)?;
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    // One entry past the page tells whether another page exists without a
    // second query.
    let mut envelopes = store
        .read_events_page(&persistence_id, request.after, request.limit + 1)
        .await
        .map_err(HistoryReadError::Storage)?;
    let next = if envelopes.len() > request.limit {
        envelopes.truncate(request.limit);
        envelopes.last().map(|envelope| envelope.sequence_nr)
    } else {
        None
    };
    let events = envelopes
        .iter()
        .map(|envelope| history_event(&persistence_id, envelope))
        .collect::<Result<Vec<_>, _>>()?;

    let span = tracing::Span::current();
    span.record("history.returned", events.len());
    span.record("history.has_next", next.is_some());
    Ok(serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": events,
        "next": next,
    }))
}

/// Shape one journal entry as a history event. Every event says who made the
/// move and the dispatch path that carried it, null on entries written before
/// the kernel recorded them.
fn history_event(
    persistence_id: &str,
    envelope: &PersistenceEnvelope,
) -> Result<serde_json::Value, HistoryReadError> {
    if envelope.event_type == COMPOSITE_EVENT_TYPE {
        let audit: CompositeAudit = decode(persistence_id, envelope)?;
        let sub_writes: Vec<_> = audit
            .composite
            .sub_writes
            .iter()
            .map(|write| {
                serde_json::json!({
                    "entity_type": write.entity_type,
                    "entity_id": write.entity_id,
                    "action": write.action,
                })
            })
            .collect();
        return Ok(serde_json::json!({
            "sequence": envelope.sequence_nr,
            "action": audit.composite.parent_action,
            "timestamp": envelope.metadata.timestamp,
            "sub_writes": sub_writes,
            "made_by": audit.made_by,
            "source": audit.source,
        }));
    }
    let event: EntityEvent = decode(persistence_id, envelope)?;
    Ok(serde_json::json!({
        "sequence": envelope.sequence_nr,
        "action": event.action,
        "from_state": event.from_status,
        "to_state": event.to_status,
        "timestamp": event.timestamp,
        "params": event.params,
        "made_by": event.made_by,
        "source": event.source,
    }))
}

/// Decode a journal entry's payload, naming the entry when it cannot be.
fn decode<T: serde::de::DeserializeOwned>(
    persistence_id: &str,
    envelope: &PersistenceEnvelope,
) -> Result<T, HistoryReadError> {
    serde_json::from_value(envelope.payload.clone()).map_err(|error| {
        HistoryReadError::Storage(PersistenceError::Serialization(format!(
            "{persistence_id} sequence {} ({}): {error}",
            envelope.sequence_nr, envelope.event_type
        )))
    })
}
