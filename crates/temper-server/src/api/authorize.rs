//! Authorization check and audit API endpoints.
//!
//! Provides lightweight Cedar authorization checks for agent tool calls and
//! records tool invocations in the trajectory log for observability.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::require_authenticated_context;
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

/// Request body for POST /api/authorize. SDK single-pair callers remain supported.
#[derive(serde::Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum AuthorizeRequest {
    Batch {
        agent_id: String,
        records: Vec<AuthorizeRecord>,
    },
    Single {
        agent_id: String,
        action: String,
        resource_type: String,
        resource_id: String,
        #[serde(default)]
        context: serde_json::Value,
    },
}

/// One record's context is prepared once and shared by all requested actions.
#[derive(serde::Deserialize)]
pub(crate) struct AuthorizeRecord {
    resource_type: String,
    resource_id: String,
    actions: Vec<String>,
    #[serde(default)]
    context: serde_json::Value,
}

/// POST /api/authorize asks a question; it never requests human approval.
///
/// A batch performs one Cedar check per action, with zero record store reads:
/// like the single-pair API, it evaluates the supplied context, not a dispatch
/// snapshot. Each record's context is prepared once. Axum's request body limit
/// bounds input bytes; there is no arbitrary record/action count cap.
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
    let security_ctx = authenticated.security_context();
    let (agent_id, records, single) = match body {
        AuthorizeRequest::Batch { agent_id, records } => (agent_id, records, false),
        AuthorizeRequest::Single {
            agent_id,
            action,
            resource_type,
            resource_id,
            context,
        } => (
            agent_id,
            vec![AuthorizeRecord {
                resource_type,
                resource_id,
                actions: vec![action],
                context,
            }],
            true,
        ),
    };
    if agent_id != security_ctx.principal.id {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "agent_id must match the authenticated principal"
            })),
        )
            .into_response();
    }
    // Validate every context before evaluating any action or writing trajectories.
    let mut prepared = Vec::with_capacity(records.len());
    for record in records {
        let mut attrs: std::collections::BTreeMap<String, serde_json::Value> = match record.context
        {
            serde_json::Value::Null => Default::default(),
            serde_json::Value::Object(context) => context.into_iter().collect(),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": "context must be a JSON object"
                    })),
                )
                    .into_response();
            }
        };
        // Caller context cannot replace the canonical Cedar resource UID.
        attrs.insert(
            "id".into(),
            serde_json::Value::String(record.resource_id.clone()),
        );
        prepared.push((
            record.resource_type,
            record.resource_id,
            record.actions,
            attrs,
        ));
    }
    let tenant = authenticated.tenant();
    let mut results = Vec::with_capacity(prepared.len());
    for (resource_type, resource_id, actions, attrs) in prepared {
        let mut answers = Vec::with_capacity(actions.len());
        for action in actions {
            let result = state.authorize_with_context(
                security_ctx,
                &action,
                &resource_type,
                &attrs,
                tenant.as_str(),
            );
            let reason = result.err().map(|denial| denial.to_string());
            if let Some(reason) = &reason {
                // Preserve the existing denial trajectory without filing a decision,
                // broadcasting an approval request, or feeding denial patterns.
                let entry = TrajectoryEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.as_str().to_string(),
                    entity_type: resource_type.clone(),
                    entity_id: resource_id.clone(),
                    action: action.clone(),
                    success: false,
                    from_status: None,
                    to_status: None,
                    error: Some(reason.clone()),
                    agent_id: Some(security_ctx.principal.id.clone()),
                    session_id: authenticated.session_id().map(str::to_string).or_else(|| {
                        security_ctx
                            .context_attrs
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    }),
                    authz_denied: Some(true),
                    denied_resource: Some(format!("{resource_type}:{resource_id}")),
                    denied_module: None,
                    source: Some(TrajectorySource::Authz),
                    spec_governed: Some(false),
                    agent_type: security_ctx.principal.agent_type.clone(),
                    request_body: Some(serde_json::json!(attrs)),
                    intent: authenticated.intent().map(str::to_string),
                    matched_policy_ids: None,
                    capture_seq: None,
                };
                if !state.enqueue_trajectory_entry(entry) {
                    tracing::warn!("failed to enqueue authorize trajectory entry");
                }
            }
            answers.push(serde_json::json!({ "action": action, "allowed": reason.is_none(), "reason": reason }));
        }
        if single {
            let answer = &answers[0];
            return (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "allowed": answer["allowed"], "reason": answer["reason"]
                })),
            )
                .into_response();
        }
        results.push(serde_json::json!({ "resource_type": resource_type, "resource_id": resource_id, "actions": answers }));
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "records": results })),
    )
        .into_response()
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
