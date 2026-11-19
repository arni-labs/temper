//! Authorization questions and audit API endpoints.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::require_authenticated_context;
use crate::state::offer::OnSnapshot;
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

/// One record snapshot, shared by all its requested moves.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeRecord {
    resource_type: String,
    resource_id: String,
    actions: Vec<String>,
    #[serde(default)]
    context: serde_json::Value,
}

/// Batched questions for one credential-derived principal, in request order.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizeBatch {
    agent_id: String,
    records: Vec<AuthorizeRecord>,
}

/// Single-pair compatibility for the published Rust and TypeScript SDKs.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizeSingle {
    agent_id: String,
    action: String,
    resource_type: String,
    resource_id: String,
    #[serde(default)]
    context: serde_json::Value,
}

/// Request body for POST /api/authorize.
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum AuthorizeRequest {
    Batch(AuthorizeBatch),
    Single(AuthorizeSingle),
}

/// Ask whether moves could be made, without requesting one. A question is a
/// read and writes nothing: no decision, denial pattern or trajectory row,
/// however many records and moves a page asks about.
///
/// A move is allowed when the record's state and the move's guard let it be
/// made (the check every move goes through, see [`crate::state::offer`]) and
/// Cedar lets this principal make it. A move its state or guard rules out
/// answers with the refusal a dispatch of it would get. Each Cedar answer is
/// counted as `check:offer` in `temper_cedar_evaluations_total`, apart from
/// the `check:gate` refusals of requests, and stays visible as the authz wide
/// event every check emits.
///
/// Each record's caller-supplied snapshot is decoded once and shared across its
/// actions; a move whose guard reads another record is decided on the stored
/// record instead, as a dispatch decides it. Cost: at most one Cedar evaluation
/// per pair and zero writes; one read of a record only when an asked move's
/// guard reads another record, plus one status lookup per such record.
/// There is no item-count cap: the router's request-body byte limit bounds input.
/// These are offers against a snapshot, not grants; dispatch authorizes again.
#[instrument(skip_all, fields(otel.name = "POST /api/authorize"))]
pub(crate) async fn handle_authorize(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<AuthorizeRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let (agent_id, records, single) = match body {
        AuthorizeRequest::Batch(batch) => (batch.agent_id, batch.records, false),
        AuthorizeRequest::Single(one) => (
            one.agent_id,
            vec![AuthorizeRecord {
                resource_type: one.resource_type,
                resource_id: one.resource_id,
                actions: vec![one.action],
                context: one.context,
            }],
            true,
        ),
    };
    let security_ctx = authenticated.security_context();
    if agent_id != security_ctx.principal.id {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "agent_id must match the authenticated principal"
            })),
        )
            .into_response();
    }
    // Validate the whole page before evaluating or emitting any trajectory.
    if records
        .iter()
        .any(|r| !r.context.is_null() && !r.context.is_object())
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "context must be a JSON object"
            })),
        )
            .into_response();
    }
    let tenant = authenticated.tenant();
    let mut answers = Vec::with_capacity(records.len());
    for record in records {
        let mut attrs: std::collections::BTreeMap<String, serde_json::Value> = match record.context
        {
            serde_json::Value::Object(context) => context.into_iter().collect(),
            _ => Default::default(),
        };
        // Caller context cannot replace the canonical Cedar UID.
        attrs.insert("id".to_string(), serde_json::json!(record.resource_id));
        // A move the snapshot's state or guard rules out is refused whoever
        // asks, so Cedar is asked only about the moves it could make.
        let on_snapshot =
            state.offers_on_snapshot(tenant, &record.resource_type, &attrs, &record.actions);
        // A guard that reads another record is decided on the stored record.
        let reads_related: Vec<String> = record
            .actions
            .iter()
            .zip(&on_snapshot)
            .filter(|(_, says)| matches!(says, OnSnapshot::ReadsRelated))
            .map(|(action, _)| action.clone())
            .collect();
        let mut on_stored = if reads_related.is_empty() {
            Vec::new()
        } else {
            state
                .offers_on_stored(
                    tenant,
                    security_ctx,
                    &record.resource_type,
                    &record.resource_id,
                    &reads_related,
                )
                .await
        }
        .into_iter();
        let mut actions = Vec::with_capacity(record.actions.len());
        for (action, says) in record.actions.into_iter().zip(on_snapshot) {
            let (reason, refusal) = match says {
                OnSnapshot::Refused(message, refusal) => (Some(message), Some(refusal)),
                // One stored answer per such move; a missing one offers nothing.
                OnSnapshot::ReadsRelated => on_stored.next().unwrap_or_else(|| {
                    (Some("the stored record gave no answer".to_string()), None)
                }),
                OnSnapshot::Free => (
                    state
                        .authorize_offer_with_context(
                            security_ctx,
                            &action,
                            &record.resource_type,
                            &attrs,
                            tenant.as_str(),
                        )
                        .err()
                        .map(|denial| denial.to_string()),
                    None,
                ),
            };
            actions.push(serde_json::json!({
                "action": action, "allowed": reason.is_none(), "reason": reason, "refusal": refusal
            }));
        }
        answers.push(serde_json::json!({
            "resource_type": record.resource_type, "resource_id": record.resource_id, "actions": actions
        }));
    }
    let answer = if single {
        let mut answer = answers.remove(0)["actions"][0].clone();
        answer
            .as_object_mut()
            .expect("single answer is an object")
            .remove("action");
        answer
    } else {
        serde_json::json!({"records": answers})
    };
    (StatusCode::OK, axum::Json(answer)).into_response()
}

/// Request body for POST /api/audit.
#[derive(serde::Deserialize)]
pub(crate) struct AuditRequest {
    agent_id: String,
    action: String,
    resource_type: String,
    resource_id: String,
    success: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    request_body: Option<serde_json::Value>,
    #[serde(default)]
    intent: Option<String>,
    /// Tool result summary (accepted for forward compatibility).
    #[serde(default)]
    #[allow(dead_code)]
    result: Option<String>,
    /// Execution duration in milliseconds (accepted for forward compatibility).
    #[serde(default)]
    #[allow(dead_code)]
    duration_ms: Option<u64>,
}

/// POST /api/audit — record a tool invocation in the trajectory log.
#[instrument(skip_all, fields(otel.name = "POST /api/audit"))]
pub(crate) async fn handle_audit(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<AuditRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let security_ctx = authenticated.security_context();
    if body.agent_id != security_ctx.principal.id {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "agent_id must match the authenticated principal"
            })),
        )
            .into_response();
    }
    let tenant = authenticated.tenant();

    let entry = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.as_str().to_string(),
        entity_type: body.resource_type,
        entity_id: body.resource_id,
        action: body.action,
        success: body.success,
        from_status: None,
        to_status: None,
        error: body.error,
        agent_id: Some(security_ctx.principal.id.clone()),
        session_id: body.session_id,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Entity),
        spec_governed: Some(false),
        agent_type: security_ctx.principal.agent_type.clone(),
        request_body: body.request_body,
        intent: body.intent,
        matched_policy_ids: None,
        capture_seq: None,
    };

    if !state.enqueue_trajectory_entry(entry) {
        tracing::warn!("failed to enqueue audit trajectory entry");
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "recorded": true })),
    )
        .into_response()
}
