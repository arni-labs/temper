//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use std::collections::BTreeMap;
use std::str::FromStr;

use axum::http::HeaderMap;
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use temper_authz::SecurityContext;
use tracing_opentelemetry::OpenTelemetrySpanExt;

mod observation_metadata;

/// Maximum callbacks along one server-owned internal context lineage.
///
/// A scheduled follow-on that waits on the clock starts a new lineage (see
/// `for_scheduled_follow_on`).
///
/// This is not a global fanout quota and is not persisted across restarts or
/// propagated through external HTTP requests.
pub const MAX_CALLBACK_HOPS: u32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CallbackBudgetExceeded {
    #[error("integration callback depth budget exhausted")]
    InlineDepth,
    #[error("integration callback hop budget exhausted")]
    TotalHops,
}

/// Agent identity context extracted from HTTP headers and credential resolution.
///
/// Threads identity through the dispatch chain for attribution in
/// trajectories, events, and WASM invocations.
///
/// Identity fields (`agent_id`, `agent_type`) are populated from the
/// credential-resolved `ResolvedIdentity` (ADR-0033), NOT from self-declared
/// headers. Only observability headers are extracted from HTTP:
/// - `X-Session-Id` / `X-Temper-Observe-Session-Id` — session grouping
/// - `X-Intent` / `X-Temper-Observe-Intent` — caller-supplied intent
/// - `X-Temper-Observe-Metadata` or `X-Temper-Observe-Meta-*` — generic,
///   namespaced observability metadata supplied by clients
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// Runtime-owned integration nesting depth; never populated from request headers.
    pub callback_depth: u32,
    /// Runtime-owned cumulative callback count; never populated from request headers.
    pub callback_hops: u32,
    /// Full Cedar security context when known at the request boundary.
    ///
    /// External HTTP entrypoints populate this after credential resolution so
    /// downstream trigger dispatch can inherit the exact principal rather than
    /// approximating it from partial agent metadata.
    pub security_ctx: Option<SecurityContext>,
    /// Optional agent identifier. Populated from `ResolvedIdentity` when
    /// credential resolution succeeds, or from internal system context.
    pub agent_id: Option<String>,
    /// Optional session identifier (from `X-Session-Id` header).
    pub session_id: Option<String>,
    /// Optional agent type classification. Populated from `ResolvedIdentity`
    /// when credential resolution succeeds.
    pub agent_type: Option<String>,
    /// Optional intent description (from `X-Intent` header).
    ///
    /// Captured on failed requests so the Evolution Engine can surface
    /// exactly what the agent was trying to accomplish.
    pub intent: Option<String>,
    /// W3C trace ID extracted from the `traceparent` header.
    /// Propagated through WASM HTTP calls to unify agent lifecycle traces.
    pub trace_id: Option<String>,
    /// Parent span ID from the `traceparent` header.
    pub parent_span_id: Option<String>,
    /// Root entity type for the logical workflow trace.
    ///
    /// This is observability metadata carried in dispatch context. It is not
    /// persisted as entity business state.
    pub workflow_root_entity_type: Option<String>,
    /// Root entity ID for the logical workflow trace.
    pub workflow_root_entity_id: Option<String>,
    /// Stable workflow run identifier used as a queryable APM/log attribute.
    pub workflow_run_id: Option<String>,
    /// ADR-0048 sub-decision 5: idempotency key extracted from the
    /// `Idempotency-Key` header. Threaded into `EntityMsg::Action` so the
    /// actor can dedupe duplicate asks produced by dispatch-layer retries.
    pub idempotency_key: Option<String>,
    /// Generic, client-supplied observability metadata.
    ///
    /// Producers should namespace their keys, for example
    /// `workflow.run_id`, `producer.work_item_id`, or `support.ticket_id`.
    /// Temper core treats these keys as opaque correlation metadata.
    pub observation_metadata: BTreeMap<String, String>,
}

impl AgentContext {
    /// Admit a callback only within both the inline and cumulative budgets.
    pub(crate) fn for_callback(&self) -> Result<Self, CallbackBudgetExceeded> {
        if self.callback_depth >= temper_runtime::reaction::MAX_REACTION_DEPTH {
            return Err(CallbackBudgetExceeded::InlineDepth);
        }
        if self.callback_hops >= MAX_CALLBACK_HOPS {
            return Err(CallbackBudgetExceeded::TotalHops);
        }
        let mut next = self.clone();
        next.callback_depth += 1;
        next.callback_hops += 1;
        Ok(next)
    }

    /// Begin a detached task without replenishing its causal callback budget.
    ///
    /// Call only at a runtime-owned spawn boundary: the parent inline stack
    /// ends there, but its callback lineage continues in the detached task.
    pub(crate) fn for_background_task(&self) -> Self {
        let mut next = self.clone();
        next.callback_depth = 0;
        next
    }

    /// Begin a `schedule` / `schedule_at` follow-on of this context.
    ///
    /// A follow-on that waits on the clock (a delay of one second or more)
    /// runs at most once per second per schedule effect, the same floor a
    /// state timer has (`state_timeout` requires `after_seconds > 0`). That
    /// wait is what bounds a periodic schedule, so it starts a fresh callback
    /// lineage and a recurring schedule keeps running however many cycles it
    /// has completed. A follow-on with no delay, including a `schedule_at`
    /// whose time has already passed, keeps the parent's context unchanged.
    pub(crate) fn for_scheduled_follow_on(&self, delay_seconds: u64) -> Self {
        let mut next = self.clone();
        if delay_seconds > 0 {
            next.callback_depth = 0;
            next.callback_hops = 0;
        }
        next
    }

    /// Create a system-level agent context for internal operations.
    ///
    /// Marks the provenance as `"system"` so that trajectories and events
    /// attribute the action to the platform itself rather than silently
    /// dropping identity via `Default`.
    pub fn system() -> Self {
        Self {
            callback_depth: 0,
            callback_hops: 0,
            security_ctx: Some(SecurityContext::system()),
            agent_id: Some("system".to_string()),
            session_id: None,
            agent_type: None,
            intent: None,
            trace_id: None,
            parent_span_id: None,
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            idempotency_key: None,
            observation_metadata: BTreeMap::new(),
        }
    }

    /// ADR-0046: Create an `AgentContext` for a named platform service.
    ///
    /// Used in place of [`AgentContext::system`] to give callers an
    /// explicit, auditable identity. The service name populates
    /// `agent_type` so Cedar policies can match on
    /// `principal.agent_type == "<service>"` — narrower than the
    /// broad-permit `system-platform` policy.
    ///
    /// Recommended service names are tracked in
    /// `docs/adrs/0046-unified-action-triggers.md`.
    pub fn for_service(service_name: &str) -> Self {
        let service_id = format!("service:{service_name}");
        let mut security_ctx = SecurityContext::anonymous().with_agent_context(
            Some(&service_id),
            None,
            Some(service_name),
        );
        security_ctx.principal.role = Some("service".to_string());

        Self {
            callback_depth: 0,
            callback_hops: 0,
            security_ctx: Some(security_ctx),
            agent_id: Some(service_id),
            session_id: None,
            agent_type: Some(service_name.to_string()),
            intent: None,
            trace_id: None,
            parent_span_id: None,
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            idempotency_key: None,
            observation_metadata: BTreeMap::new(),
        }
    }

    /// Create a system-level context for a named internal transport or adapter.
    pub fn system_with_agent_id(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: Some(agent_id.into()),
            agent_type: Some("system".to_string()),
            ..Self::default()
        }
    }

    /// Create a service identity while preserving caller observability context.
    ///
    /// Service callbacks often need a narrower principal than the invoking
    /// agent, but they still belong to the same logical workflow trace.
    pub fn for_service_inheriting(service_name: &str, parent: &AgentContext) -> Self {
        Self::for_service(service_name).inherit_observability_from(parent)
    }

    /// Copy the execution budget and non-authority observability fields from a parent.
    pub fn inherit_observability_from(mut self, parent: &AgentContext) -> Self {
        self.callback_depth = parent.callback_depth;
        self.callback_hops = parent.callback_hops;
        self.session_id = parent.session_id.clone();
        self.intent = parent.intent.clone();
        self.trace_id = parent.trace_id.clone();
        self.parent_span_id = parent.parent_span_id.clone();
        self.workflow_root_entity_type = parent.workflow_root_entity_type.clone();
        self.workflow_root_entity_id = parent.workflow_root_entity_id.clone();
        self.workflow_run_id = parent.workflow_run_id.clone();
        self.observation_metadata = parent.observation_metadata.clone();
        self
    }

    /// Ensure this context has a workflow root and current-span trace ids.
    pub fn for_dispatch_root(&self, entity_type: &str, entity_id: &str) -> Self {
        let mut next = self.clone();
        if next.workflow_root_entity_type.is_none() {
            next.workflow_root_entity_type = Some(entity_type.to_string());
        }
        if next.workflow_root_entity_id.is_none() {
            next.workflow_root_entity_id = Some(entity_id.to_string());
        }
        if next.workflow_run_id.is_none() {
            let root_type = next
                .workflow_root_entity_type
                .as_deref()
                .unwrap_or(entity_type);
            let root_id = next.workflow_root_entity_id.as_deref().unwrap_or(entity_id);
            next.workflow_run_id = Some(format!("{root_type}:{root_id}"));
        }
        next.with_current_span_trace_context()
    }

    /// Fill missing W3C trace IDs from the currently active OpenTelemetry span.
    pub fn with_current_span_trace_context(mut self) -> Self {
        if let Some((trace_id, span_id)) = current_span_trace_context_ids() {
            self.trace_id = Some(trace_id);
            self.parent_span_id = Some(span_id);
        }
        self
    }

    /// Serialize observation metadata for log fields.
    pub fn observation_metadata_json(&self) -> Option<String> {
        if self.observation_metadata.is_empty() {
            return None;
        }
        serde_json::to_string(&self.observation_metadata).ok()
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Extract the caller-supplied session id from observability headers.
///
/// Accepts `X-Temper-Observe-Session-Id` and the shorter `X-Session-Id` alias.
/// Single source of truth so every entrypoint honours both spellings.
pub fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    header_string(headers, "x-temper-observe-session-id")
        .or_else(|| header_string(headers, "x-session-id"))
}

/// Extract the caller-supplied intent from observability headers.
///
/// Accepts `X-Temper-Observe-Intent` and the shorter `X-Intent` alias.
pub fn intent_from_headers(headers: &HeaderMap) -> Option<String> {
    header_string(headers, "x-temper-observe-intent").or_else(|| header_string(headers, "x-intent"))
}

/// Extract observability context from request headers.
///
/// Reads generic session, intent, and observation metadata headers for
/// observability purposes.
/// Identity fields (`agent_id`, `agent_type`) are NOT extracted from
/// self-declared headers — they come from credential resolution (ADR-0033)
/// or are set to `None` for anonymous/operator access.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    let session_id = session_id_from_headers(headers);
    let intent = intent_from_headers(headers);
    let (trace_id, parent_span_id) = traceparent_ids(headers)
        .map(|(t, s)| (Some(t), Some(s)))
        .unwrap_or((None, None));

    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let workflow_root_entity_type = header_string(headers, "x-temper-workflow-root-entity-type");
    let workflow_root_entity_id = header_string(headers, "x-temper-workflow-root-entity-id");
    let workflow_run_id = header_string(headers, "x-temper-workflow-run-id");

    AgentContext {
        callback_depth: 0,
        callback_hops: 0,
        security_ctx: None,
        agent_id: None,
        session_id,
        agent_type: None,
        intent,
        trace_id,
        parent_span_id,
        workflow_root_entity_type,
        workflow_root_entity_id,
        workflow_run_id,
        idempotency_key,
        observation_metadata: observation_metadata::extract(headers),
    }
}

pub(crate) fn current_span_trace_context_ids() -> Option<(String, String)> {
    let span_context = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .clone();
    if !span_context.is_valid() {
        return None;
    }
    Some((
        span_context.trace_id().to_string(),
        span_context.span_id().to_string(),
    ))
}

/// The trace and parent span ids of a W3C `traceparent` header:
/// `00-{trace_id}-{parent_span_id}-{flags}`.
fn traceparent_ids(headers: &HeaderMap) -> Option<(String, String)> {
    let traceparent = headers.get("traceparent")?.to_str().ok()?;
    let parts: Vec<&str> = traceparent.split('-').collect();
    (parts.len() >= 4 && parts[1].len() == 32 && parts[2].len() == 16)
        .then(|| (parts[1].to_string(), parts[2].to_string()))
}

/// The caller's span from a request's `traceparent` header, as the parent of the
/// request's server span. Only the server span takes it: everything the request
/// does is then in that one trace. The caller's sampling decision and its
/// `tracestate` are kept, so a trace the caller did not sample stays unsampled.
/// The caller's own copy (`x-temper-traceparent`) is read first: a proxy between
/// the two may have put its own span into `traceparent`. A Temper that always
/// sits behind such a proxy sets `TEMPER_TRUST_TRACEPARENT=false`: only the copy
/// names a parent there, and a request without one starts its own trace instead
/// of hanging from the proxy's span, which never reaches the trace.
pub(crate) fn remote_parent_from_headers(headers: &HeaderMap) -> Option<opentelemetry::Context> {
    static TRUST_TRACEPARENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let trust = *TRUST_TRACEPARENT.get_or_init(|| {
        std::env::var("TEMPER_TRUST_TRACEPARENT") // determinism-ok: read once at startup
            .map(|value| !value.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(true)
    });
    remote_parent_from_trusted_headers(headers, trust)
}

pub(crate) fn remote_parent_from_trusted_headers(
    headers: &HeaderMap,
    trust_traceparent: bool,
) -> Option<opentelemetry::Context> {
    let names: &[&str] = if trust_traceparent {
        &[temper_observe::otel::TRACEPARENT_COPY, "traceparent"]
    } else {
        &[temper_observe::otel::TRACEPARENT_COPY]
    };
    names
        .iter()
        .find_map(|name| remote_parent_from_header(headers, name))
}

fn remote_parent_from_header(headers: &HeaderMap, name: &str) -> Option<opentelemetry::Context> {
    let traceparent = headers.get(name)?.to_str().ok()?.trim();
    let parts: Vec<&str> = traceparent.split('-').collect();
    let [version, trace_id, span_id, flags, rest @ ..] = parts.as_slice() else {
        return None;
    };
    // W3C trace context: version ff is invalid, and version 00 has exactly four
    // fields; a later version may add fields after them.
    if version.len() != 2 || *version == "ff" || (*version == "00" && !rest.is_empty()) {
        return None;
    }
    if flags.len() != 2 {
        return None;
    }
    let flags = TraceFlags::new(u8::from_str_radix(flags, 16).ok()?);
    let state = headers
        .get("tracestate")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| TraceState::from_str(value).ok())
        .unwrap_or_default();
    remote_parent(trace_id, span_id, flags, state)
}

pub(crate) fn remote_parent_context(agent_ctx: &AgentContext) -> Option<opentelemetry::Context> {
    remote_parent_from_ids(
        agent_ctx.trace_id.as_deref()?,
        agent_ctx.parent_span_id.as_deref()?,
    )
}

fn remote_parent_from_ids(trace_id: &str, span_id: &str) -> Option<opentelemetry::Context> {
    remote_parent(
        trace_id,
        span_id,
        TraceFlags::SAMPLED,
        TraceState::default(),
    )
}

fn remote_parent(
    trace_id: &str,
    span_id: &str,
    flags: TraceFlags,
    state: TraceState,
) -> Option<opentelemetry::Context> {
    let trace_id = TraceId::from_hex(trace_id).ok()?;
    let span_id = SpanId::from_hex(span_id).ok()?;
    if trace_id == TraceId::INVALID || span_id == SpanId::INVALID {
        return None;
    }
    let span_context = SpanContext::new(trace_id, span_id, flags, true, state);
    Some(opentelemetry::Context::new().with_remote_span_context(span_context))
}

#[cfg(test)]
#[path = "request_context/mod_test.rs"]
mod tests;

#[cfg(test)]
#[path = "request_context/callback_budget_test.rs"]
mod callback_budget_tests;
