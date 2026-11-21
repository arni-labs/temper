//! Record change subscriptions use the same Cedar read boundary as OData.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use temper_authz::AuthenticatedRequestContext;
use temper_odata::query::filter::parse_filter;
use temper_odata::query::types::FilterExpr;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::{Instant, timeout_at};
use tracing::instrument;

use super::RecordChange;
use crate::authz::{require_authenticated_context, require_observe_auth};
use crate::blobs::{BlobHydrationBudget, hydrate_blob_refs_for_tenant_with_budget};
use crate::identity::{credential_revoked, refresh_principal_records};
use crate::odata::authz::{READ_ACTION, authorize_read};
use crate::odata::read_defaults::ReadDefaults;
use crate::odata::{handle_odata_get_for_tenant, resolve_entity_type};
use crate::query_eval::evaluate_filter;
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

#[derive(Deserialize)]
pub(crate) struct SubscriptionQuery {
    pub(super) sets: Option<String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    observe: bool,
    #[serde(default)]
    only_changes: bool,
    #[serde(rename = "$filter")]
    filter: Option<String>,
}

struct Subscription {
    state: ServerState,
    authenticated: AuthenticatedRequestContext,
    receiver: broadcast::Receiver<RecordChange>,
    types: BTreeMap<String, String>,
    filter: Option<FilterExpr>,
    deadline: Instant,
    observe_metadata: bool,
}

enum Outcome {
    Changed(Value),
    Recheck(String),
    TimedOut,
    Interrupted,
}

fn invalid_query(message: &str) -> Response {
    odata_error(StatusCode::BAD_REQUEST, "InvalidQuery", message).into_response()
}

impl Subscription {
    fn open(
        state: ServerState,
        authenticated: AuthenticatedRequestContext,
        query: &SubscriptionQuery,
    ) -> Result<Self, Response> {
        let Some(sets) = query.sets.as_ref().filter(|sets| !sets.is_empty()) else {
            return Err(invalid_query("Send sets with one or more entity set names"));
        };
        let Some(deadline) = query
            .timeout_ms
            .filter(|ms| *ms > 0)
            .and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms)))
        else {
            return Err(invalid_query("Send a positive timeout_ms"));
        };
        let mut types = BTreeMap::new();
        for set in sets.split(',') {
            let Some(entity_type) = resolve_entity_type(&state, authenticated.tenant(), set) else {
                return Err(odata_error(
                    StatusCode::NOT_FOUND,
                    "EntitySetNotFound",
                    "The subscription names an unknown entity set",
                )
                .into_response());
            };
            types.insert(entity_type, set.to_string());
        }
        let filter = query
            .filter
            .as_deref()
            .map(parse_filter)
            .transpose()
            .map_err(|err| invalid_query(&err.to_string()))?;
        let receiver = state.record_event_tx.subscribe();
        Ok(Self {
            state,
            authenticated,
            receiver,
            types,
            filter,
            deadline,
            observe_metadata: false,
        })
    }

    async fn next(&mut self) -> Outcome {
        match timeout_at(self.deadline, self.next_change()).await {
            Ok(outcome) => outcome,
            Err(_) => Outcome::TimedOut,
        }
    }

    async fn next_change(&mut self) -> Outcome {
        loop {
            let RecordChange {
                change,
                record: snapshot,
            } = match self.receiver.recv().await {
                Ok(change) => change,
                Err(RecvError::Closed | RecvError::Lagged(_)) => return Outcome::Interrupted,
            };
            let tenant = self.authenticated.tenant();
            if tenant.as_str() != change.tenant {
                continue;
            }
            if !self.types.contains_key(&change.entity_type) {
                if self.observe_metadata {
                    return Outcome::Changed(json!(change));
                }
                continue;
            }
            let mut record = (*snapshot).clone();
            ReadDefaults::for_entity(&self.state, tenant, &change.entity_type).apply(&mut record);
            if self
                .filter
                .as_ref()
                .is_some_and(|filter| evaluate_filter(&record, filter) != Some(true))
            {
                continue;
            }
            let context = self.authenticated.security_context();
            if let Some(credential) = &context.credential
                && !matches!(
                    credential_revoked(&self.state, tenant, credential).await,
                    Ok(None)
                )
            {
                return Outcome::Interrupted;
            }
            let Ok(context) = refresh_principal_records(&self.state, tenant, context).await else {
                return Outcome::Interrupted;
            };
            let authorized = authorize_read(
                &self.state,
                tenant,
                &context,
                READ_ACTION,
                &change.entity_type,
                &change.entity_id,
                &record,
            )
            .is_ok();
            if !authorized || record["last_event_tombstone"] == true {
                // A previously displayed record may need to leave a page. Only
                // callers who may observe metadata can hear that a type changed
                // when the new row itself is unreadable.
                if self.observe_metadata
                    && !matches!(change.action.as_str(), "Created" | "Add")
                    && self
                        .state
                        .authorize_with_context(
                            &context,
                            "read_events",
                            "Entity",
                            &BTreeMap::new(),
                            tenant.as_str(),
                        )
                        .is_ok()
                {
                    return Outcome::Recheck(change.entity_type);
                }
                continue;
            }
            let budget = BlobHydrationBudget::generic_response();
            hydrate_blob_refs_for_tenant_with_budget(&self.state, tenant, &mut record, &budget)
                .await;
            return Outcome::Changed(json!({
                "seq": change.seq,
                "entity_type": change.entity_type,
                "entity_id": change.entity_id,
                "action": change.action,
                "status": change.status,
                "record": record,
            }));
        }
    }

    // Use one ordinary authorized list read to check an already-true
    // condition. The receiver is already subscribed when the read begins.
    async fn existing(&self, filter: String) -> Result<Option<Value>, Response> {
        let Some(set) = self.types.values().next().filter(|_| self.types.len() == 1) else {
            return Err(invalid_query(
                "$filter on a wait requires exactly one entity set",
            ));
        };
        let response = handle_odata_get_for_tenant(
            self.state.clone(),
            self.authenticated.tenant().clone(),
            self.authenticated.security_context().clone(),
            set.clone(),
            BTreeMap::from([("$filter".into(), filter), ("$top".into(), "1".into())]),
        )
        .await;
        if !response.status().is_success() {
            return Err(response);
        }
        let body = to_bytes(response.into_body(), usize::MAX).await.ok();
        let list = body
            .as_ref()
            .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
        let Some(Value::Array(records)) =
            list.and_then(|mut list| list.get_mut("value").map(Value::take))
        else {
            return Err(odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ReadUnavailable",
                "The initial condition read did not return a page",
            )
            .into_response());
        };
        Ok(records.into_iter().next())
    }
}

pub(super) fn stream(
    state: ServerState,
    authenticated: AuthenticatedRequestContext,
    query: SubscriptionQuery,
) -> Response {
    if query.observe
        && let Err(status) = require_observe_auth(&state, &authenticated, "read_events", "Entity")
    {
        return status.into_response();
    }
    let mut subscription = match Subscription::open(state, authenticated, &query) {
        Ok(subscription) => subscription,
        Err(response) => return response,
    };
    subscription.observe_metadata = query.observe;
    let stream = async_stream::stream! {
        loop {
            match subscription.next().await {
                Outcome::Changed(change) => {
                    yield Ok::<_, Infallible>(Event::default().event("state_change").data(change.to_string()));
                }
                Outcome::Recheck(entity_type) => {
                    yield Ok(Event::default().event("recheck").data(json!({ "entity_type": entity_type }).to_string()));
                }
                Outcome::TimedOut => break,
                Outcome::Interrupted => {
                    yield Ok(Event::default().event("interrupted").data("{}"));
                    break;
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Wait for a readable record change, or for a filtered condition to hold.
#[instrument(skip_all, fields(otel.name = "GET /tdata/$wait"))]
pub(crate) async fn handle_wait(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(mut query): Query<SubscriptionQuery>,
) -> Response {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated.clone(),
        Err(status) => return status.into_response(),
    };
    let mut subscription = match Subscription::open(state, authenticated, &query) {
        Ok(subscription) => subscription,
        Err(response) => return response,
    };
    if !query.only_changes
        && let Some(filter) = query.filter.take()
    {
        match timeout_at(subscription.deadline, subscription.existing(filter)).await {
            Ok(Ok(Some(record))) => return answer(json!({"outcome": "matched", "record": record})),
            Ok(Ok(None)) => {}
            Ok(Err(response)) => return response,
            Err(_) => return answer(json!({"outcome": "timed_out"})),
        }
    }
    answer(match subscription.next().await {
        Outcome::Changed(mut change) => {
            change["outcome"] = json!("matched");
            change
        }
        Outcome::TimedOut => json!({"outcome": "timed_out"}),
        Outcome::Interrupted | Outcome::Recheck(_) => json!({"outcome": "interrupted"}),
    })
}

fn answer(body: Value) -> Response {
    ODataResponse {
        status: StatusCode::OK,
        body,
    }
    .into_response()
}
