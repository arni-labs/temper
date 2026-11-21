//! Server-Sent Events (SSE) for entity state change subscriptions.
//!
//! Provides a `/tdata/$events` endpoint that streams real-time entity
//! state transitions to connected clients via SSE.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_authenticated_context, require_observe_auth};
use crate::state::ServerState;

mod scoped;
pub(crate) use scoped::handle_wait;

/// A notification emitted when an entity transitions to a new state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct EntityStateChange {
    /// Monotonic per-entity event sequence.
    #[serde(default)]
    pub seq: u64,
    /// The entity type (e.g., "Order").
    pub entity_type: String,
    /// The entity ID.
    pub entity_id: String,
    /// The action that triggered the transition.
    pub action: String,
    /// The new status after the transition.
    pub status: String,
    /// The tenant that owns the entity.
    pub tenant: String,
    /// Agent that performed the action (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session in which the action was performed (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Caller intent for the action (if supplied).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Generic metadata for correlating runtime events with producer-specific context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_metadata: Option<BTreeMap<String, String>>,
}

/// A commit and the record descriptor that Cedar can authorize on delivery.
#[derive(Clone, Debug)]
pub(crate) struct RecordChange {
    pub(crate) change: EntityStateChange,
    pub(crate) record: Arc<serde_json::Value>,
}

pub(crate) async fn handle_events_request(
    state: State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(query): Query<scoped::SubscriptionQuery>,
) -> Result<Response, StatusCode> {
    if query.sets.is_some() {
        let authenticated = require_authenticated_context(authenticated.as_deref())?;
        return Ok(scoped::stream(state.0, authenticated.clone(), query));
    }
    Ok(handle_events(state, authenticated).await?.into_response())
}

/// SSE handler for the tenant-wide, metadata-only observer stream.
///
/// Clients connect to `/tdata/$events` and receive a stream of JSON events
/// for every successful entity state transition.
#[instrument(skip_all, fields(otel.name = "GET /tdata/$events"))]
pub async fn handle_events(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_events", "Entity")?;
    let filter_tenant = observe_tenant_scope(authenticated).as_str().to_string();
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(change) => {
                // Enforce tenant scope: only emit events for the scoped tenant.
                if change.tenant != filter_tenant {
                    return None;
                }
                let data = serde_json::to_string(&change).unwrap_or_default();
                Some(Ok::<_, Infallible>(
                    Event::default().event("state_change").data(data),
                ))
            }
            // Lagged receiver: skip missed events and continue
            Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

impl ServerState {
    pub(crate) fn publish_crud_change(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        state: &crate::EntityState,
        action: &str,
    ) {
        self.publish_entity_change(
            EntityStateChange {
                seq: self.next_entity_event_sequence(
                    tenant.as_str(),
                    &state.entity_type,
                    &state.entity_id,
                ),
                entity_type: state.entity_type.clone(),
                entity_id: state.entity_id.clone(),
                action: action.to_string(),
                status: state.status.clone(),
                tenant: tenant.to_string(),
                ..EntityStateChange::default()
            },
            Some(state),
        );
    }

    /// Publish a commit alongside the existing metadata-only observation event.
    pub(crate) fn publish_entity_change(
        &self,
        change: EntityStateChange,
        committed: Option<&crate::EntityState>,
    ) {
        self.record_entity_observe_event_with_seq(
            &change.tenant,
            &change.entity_type,
            &change.entity_id,
            change.seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let Some(state) = committed.filter(|_| self.record_event_tx.receiver_count() != 0) else {
            let _ = self.event_tx.send(change);
            return;
        };
        let _ = self.event_tx.send(change.clone());
        // Carry the row, without the replay log or idempotency history. Cedar
        // authorizes this descriptor before any overflow values are hydrated.
        let mut record = serde_json::json!({
            "entity_type": state.entity_type,
            "entity_id": state.entity_id,
            "status": state.status,
            "fields": state.fields,
            "counters": state.counters,
            "booleans": state.booleans,
            "lists": state.lists,
            "item_count": state.item_count,
            "status_clock": state.status_clock,
            "sequence_nr": state.sequence_nr,
        });
        if let (Some(row), Ok(serde_json::Value::Object(metadata))) = (
            record.as_object_mut(),
            serde_json::to_value(&state.last_event),
        ) {
            row.extend(metadata);
        }
        let _ = self.record_event_tx.send(RecordChange {
            change,
            record: Arc::new(record),
        });
    }
}
