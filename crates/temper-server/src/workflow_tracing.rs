use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use opentelemetry::trace::{SpanContext, TraceContextExt};
use tracing_opentelemetry::OpenTelemetrySpanExt;

const WORKFLOW_ROOT_DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Finished workflow runs remembered so late work links to their trace instead
/// of opening a second trace for the same run.
const FINISHED_RUNS_REMEMBERED: usize = 4_096;

#[derive(Debug)]
struct WorkflowRootSpan {
    root_entity_type: String,
    root_entity_id: String,
    span: tracing::Span,
}

/// Keeps workflow root spans open across asynchronous entity actions.
#[derive(Debug, Default)]
pub(crate) struct WorkflowSpanRegistry {
    spans: Mutex<BTreeMap<String, WorkflowRootSpan>>,
    finished: Mutex<FinishedRuns>,
}

/// The root span contexts of recently finished workflow runs, oldest first.
#[derive(Debug, Default)]
struct FinishedRuns {
    order: VecDeque<String>,
    roots: BTreeMap<String, SpanContext>,
}

impl FinishedRuns {
    fn remember(&mut self, run_id: &str, root: SpanContext) {
        if self.roots.insert(run_id.to_string(), root).is_none() {
            self.order.push_back(run_id.to_string());
        }
        while self.order.len() > FINISHED_RUNS_REMEMBERED {
            if let Some(oldest) = self.order.pop_front() {
                self.roots.remove(&oldest);
            }
        }
    }
}

/// Where a workflow dispatch's work goes.
pub(crate) enum WorkflowParent {
    /// The run's open root span: the work is a step in the workflow's trace.
    Open(opentelemetry::Context),
    /// The run already finished: the work stays in its request's trace, linked
    /// to the finished workflow's root span.
    Finished(SpanContext),
    /// No trace to join (tracing is off).
    None,
}

impl WorkflowSpanRegistry {
    pub(crate) fn parent_context(
        &self,
        tenant: &str,
        root_entity_type: &str,
        root_entity_id: &str,
        workflow_run_id: &str,
        session_id: Option<&str>,
    ) -> WorkflowParent {
        let Ok(mut spans) = self.spans.lock() else {
            return WorkflowParent::None;
        };
        if let Some(root) = spans.get(workflow_run_id) {
            let context = root.span.context();
            if context.span().span_context().is_valid() {
                return WorkflowParent::Open(context);
            }
            spans.remove(workflow_run_id);
        }
        if let Some(root) = self
            .finished
            .lock()
            .ok()
            .and_then(|finished| finished.roots.get(workflow_run_id).cloned())
        {
            return WorkflowParent::Finished(root);
        }

        let span = tracing::info_span!(
            parent: None,
            "temper.workflow",
            otel.name = %format_args!("{root_entity_type}.workflow"),
            tenant = %tenant,
            entity_type = %root_entity_type,
            entity_id = %root_entity_id,
            workflow.root_entity_type = %root_entity_type,
            workflow.root_entity_id = %root_entity_id,
            workflow.run_id = %workflow_run_id,
            workflow.terminal_status = tracing::field::Empty,
            workflow.drain_grace_ms = tracing::field::Empty,
            workflow.drain_reason = tracing::field::Empty,
            session.id = session_id.unwrap_or(""),
        );
        let context = span.context();
        if !context.span().span_context().is_valid() {
            return WorkflowParent::None;
        }

        spans.insert(
            workflow_run_id.to_string(),
            WorkflowRootSpan {
                root_entity_type: root_entity_type.to_string(),
                root_entity_id: root_entity_id.to_string(),
                span,
            },
        );
        WorkflowParent::Open(context)
    }

    pub(crate) fn finish_if_terminal(
        &self,
        workflow_run_id: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
    ) -> bool {
        if !is_terminal_status(status) {
            return false;
        }

        let Ok(mut spans) = self.spans.lock() else {
            return false;
        };

        let should_finish = spans.get(workflow_run_id).is_some_and(|root| {
            root.root_entity_type == entity_type && root.root_entity_id == entity_id
        });
        if should_finish && let Some(root) = spans.remove(workflow_run_id) {
            let root_context = root.span.context().span().span_context().clone();
            if root_context.is_valid()
                && let Ok(mut finished) = self.finished.lock()
            {
                finished.remember(workflow_run_id, root_context);
            }
        }
        should_finish
    }

    fn mark_terminal_drain_scheduled(
        &self,
        workflow_run_id: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
    ) {
        let Ok(spans) = self.spans.lock() else {
            return;
        };
        let Some(root) = spans.get(workflow_run_id) else {
            return;
        };
        if root.root_entity_type != entity_type || root.root_entity_id != entity_id {
            return;
        }

        root.span.record("workflow.terminal_status", status);
        root.span.record(
            "workflow.drain_grace_ms",
            workflow_root_drain_grace_ms() as i64,
        );
        root.span
            .record("workflow.drain_reason", "post_dispatch_telemetry");
    }

    pub(crate) fn finish_if_terminal_after_drain(
        self: &std::sync::Arc<Self>,
        workflow_run_id: String,
        entity_type: String,
        entity_id: String,
        status: String,
    ) {
        if !is_terminal_status(&status) {
            return;
        }

        let registry = std::sync::Arc::clone(self);
        registry.mark_terminal_drain_scheduled(&workflow_run_id, &entity_type, &entity_id, &status);
        tokio::spawn(async move {
            // determinism-ok: observability-only root span cleanup after
            // post-dispatch telemetry has had a chance to attach. The wait is not
            // a span of its own: the workflow trace shows only its work.
            tokio::time::sleep(WORKFLOW_ROOT_DRAIN_GRACE).await;
            registry.finish_if_terminal(&workflow_run_id, &entity_type, &entity_id, &status);
        });
    }
}

fn workflow_root_drain_grace_ms() -> u64 {
    WORKFLOW_ROOT_DRAIN_GRACE.as_millis() as u64
}

pub(crate) fn should_use_workflow_root_span(
    explicit_workflow_context: bool,
    entity_type: &str,
) -> bool {
    explicit_workflow_context
        || matches!(
            entity_type,
            "Session" | "ManagedSession" | "WorkCycle" | "AlertCycle"
        )
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "Completed" | "Failed" | "Cancelled" | "Terminated")
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::TraceContextExt;

    use super::WorkflowParent;
    use crate::test_spans;

    fn open(parent: WorkflowParent) -> opentelemetry::Context {
        match parent {
            WorkflowParent::Open(context) => context,
            _ => panic!("expected an open workflow root"),
        }
    }

    #[test]
    fn workflow_root_span_is_its_own_trace_not_the_requests() {
        let _spans = test_spans::capture();
        let registry = super::WorkflowSpanRegistry::default();
        let http_span = tracing::info_span!("http.request");
        let http_trace_id = crate::trace_context::span_context(&http_span).trace_id();

        let workflow = open(http_span.in_scope(|| {
            registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1"))
        }));
        assert_ne!(workflow.span().span_context().trace_id(), http_trace_id);
        // The same run joins the same open root.
        let again =
            open(registry.parent_context("default", "Session", "ss-1", "Session:ss-1", None));
        assert_eq!(again.span().span_context(), workflow.span().span_context());
        assert!(registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));
    }

    #[test]
    fn a_finished_run_never_opens_a_second_workflow_trace() {
        let _spans = test_spans::capture();
        let registry = super::WorkflowSpanRegistry::default();
        let root =
            open(registry.parent_context("default", "Session", "ss-1", "Session:ss-1", None));
        assert!(registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));

        // A late callback for the same run: linked to the finished root, no new root.
        match registry.parent_context("default", "Session", "ss-1", "Session:ss-1", None) {
            WorkflowParent::Finished(finished) => {
                assert_eq!(&finished, root.span().span_context());
            }
            _ => panic!("a finished run must not open a second root"),
        }
        assert!(registry.spans.lock().expect("spans lock").is_empty());
    }

    #[test]
    fn workflow_root_span_is_only_used_for_workflow_context_or_agent_cycles() {
        assert!(super::should_use_workflow_root_span(false, "Session"));
        assert!(super::should_use_workflow_root_span(
            false,
            "ManagedSession"
        ));
        assert!(super::should_use_workflow_root_span(false, "WorkCycle"));
        assert!(super::should_use_workflow_root_span(false, "AlertCycle"));
        assert!(super::should_use_workflow_root_span(true, "Invoice"));
        assert!(!super::should_use_workflow_root_span(false, "Invoice"));
    }

    #[test]
    fn workflow_root_span_closes_only_on_root_terminal_status() {
        let _spans = test_spans::capture();
        let registry = super::WorkflowSpanRegistry::default();
        open(registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1")));

        assert!(!registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Running"));
        assert!(!registry.finish_if_terminal("Session:ss-1", "Child", "child-1", "Completed"));
        assert!(registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));
        assert!(!registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));
    }

    #[test]
    fn workflow_root_drain_grace_is_explicit() {
        assert_eq!(super::workflow_root_drain_grace_ms(), 2_000);
    }

    #[tokio::test]
    async fn the_drain_grace_is_not_a_span() {
        let spans = test_spans::capture();
        let registry = std::sync::Arc::new(super::WorkflowSpanRegistry::default());
        open(registry.parent_context("default", "Session", "ss-1", "Session:ss-1", None));
        tokio::time::pause();
        registry.finish_if_terminal_after_drain(
            "Session:ss-1".into(),
            "Session".into(),
            "ss-1".into(),
            "Completed".into(),
        );
        // Paused time moves on while every task waits, so the drain task runs.
        tokio::time::sleep(super::WORKFLOW_ROOT_DRAIN_GRACE * 2).await;
        let names: Vec<_> = spans
            .finished()
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec!["Session.workflow".to_string()],
            "only the workflow root: {names:?}"
        );
    }

    #[test]
    fn workflow_root_span_is_not_retained_without_otel_context() {
        let registry = super::WorkflowSpanRegistry::default();

        let parent =
            registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1"));

        assert!(matches!(parent, WorkflowParent::None));
        assert!(registry.spans.lock().expect("spans lock").is_empty());
    }
}
