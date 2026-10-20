//! One piece of work is one span in one trace.
//!
//! A request's work stays in the request's trace, including the work it queues
//! for later: the queue keeps the trace context of the span that queued an item
//! ([`capture`]), and the worker's span for that item continues it
//! ([`continue_in`]). Work that already runs inside a span never re-parents
//! itself onto a context it was handed: a span is only ever in the trace it was
//! created in, so nothing appears in two traces and no trace has two roots.
//! Work that belongs to a different trace (an agent workflow, a late callback)
//! is connected with span links instead ([`link_both`], [`link_if_other_trace`]).

use opentelemetry::trace::{SpanContext, TraceContextExt};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The current span's trace context, to be continued by work that runs later on
/// another task.
pub(crate) fn capture() -> opentelemetry::Context {
    tracing::Span::current().context()
}

/// Continue `parent` (captured with [`capture`] where the work was queued) in
/// `span`, a span the worker created without a parent and has not entered yet.
pub(crate) fn continue_in(span: &tracing::Span, parent: &opentelemetry::Context) {
    if parent.span().span_context().is_valid() {
        span.set_parent(parent.clone());
    }
}

/// Link two spans of different traces to each other.
pub(crate) fn link_both(a: &tracing::Span, b: &tracing::Span) {
    let (a_context, b_context) = (span_context(a), span_context(b));
    if b_context.is_valid() {
        a.add_link(b_context);
    }
    if a_context.is_valid() {
        b.add_link(a_context);
    }
}

/// Link `span` to `remote` when `remote` is a span of another trace. `span`
/// keeps its own parent.
pub(crate) fn link_if_other_trace(span: &tracing::Span, remote: &opentelemetry::Context) {
    let remote = remote.span().span_context().clone();
    let own = span_context(span);
    if remote.is_valid() && own.is_valid() && remote.trace_id() != own.trace_id() {
        span.add_link(remote);
    }
}

/// The W3C `traceparent` header naming `span`, for an outgoing request made
/// inside it.
pub(crate) fn traceparent(span: &tracing::Span) -> Option<String> {
    let context = span_context(span);
    context.is_valid().then(|| {
        format!(
            "00-{}-{}-{:02x}",
            context.trace_id(),
            context.span_id(),
            context.trace_flags().to_u8()
        )
    })
}

pub(crate) fn span_context(span: &tracing::Span) -> SpanContext {
    span.context().span().span_context().clone()
}

#[cfg(test)]
#[path = "trace_context_test.rs"]
mod tests;
