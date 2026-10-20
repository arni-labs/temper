use opentelemetry::trace::SpanId;

use super::*;
use crate::test_spans;

#[tokio::test]
async fn queued_work_continues_the_trace_that_queued_it() {
    let spans = test_spans::capture();
    let request = tracing::info_span!("request");
    let parent = request.in_scope(capture);
    let request_context = span_context(&request);
    drop(request);

    tokio::spawn(async move {
        let worker = tracing::info_span!(parent: None, "worker.item");
        continue_in(&worker, &parent);
        worker.in_scope(|| tracing::info_span!("worker.write").in_scope(|| {}));
    })
    .await
    .unwrap();

    let finished = spans.finished();
    let item = finished.iter().find(|s| s.name == "worker.item").unwrap();
    let write = finished.iter().find(|s| s.name == "worker.write").unwrap();
    assert_eq!(item.span_context.trace_id(), request_context.trace_id());
    assert_eq!(item.parent_span_id, request_context.span_id());
    assert_eq!(write.span_context.trace_id(), request_context.trace_id());
    assert_eq!(write.parent_span_id, item.span_context.span_id());
}

#[tokio::test]
async fn a_worker_span_with_nothing_to_continue_is_its_own_root() {
    let spans = test_spans::capture();
    let worker = tracing::info_span!(parent: None, "worker.item");
    continue_in(&worker, &opentelemetry::Context::new());
    drop(worker);
    let finished = spans.finished();
    assert_eq!(finished[0].parent_span_id, SpanId::INVALID);
}

#[test]
fn spans_of_two_traces_link_to_each_other() {
    let spans = test_spans::capture();
    let request = tracing::info_span!(parent: None, "request");
    let workflow = tracing::info_span!(parent: None, "workflow.step");
    let (request_context, workflow_context) = (span_context(&request), span_context(&workflow));
    assert_ne!(request_context.trace_id(), workflow_context.trace_id());
    link_both(&request, &workflow);
    drop((request, workflow));

    let finished = spans.finished();
    let linked = |name: &str| {
        finished
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .links
            .iter()
            .map(|link| link.span_context.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(linked("request"), vec![workflow_context]);
    assert_eq!(linked("workflow.step"), vec![request_context]);
}

#[test]
fn a_context_from_the_same_trace_is_not_linked() {
    let spans = test_spans::capture();
    let request = tracing::info_span!("request");
    let same = request.in_scope(capture);
    let other_root = tracing::info_span!(parent: None, "other");
    let other = other_root.context();
    request.in_scope(|| {
        let dispatch = tracing::info_span!("dispatch");
        link_if_other_trace(&dispatch, &same);
        link_if_other_trace(&dispatch, &other);
    });
    drop((request, other_root));
    let finished = spans.finished();
    let dispatch = finished.iter().find(|s| s.name == "dispatch").unwrap();
    let links: Vec<_> = dispatch
        .links
        .iter()
        .map(|l| l.span_context.clone())
        .collect();
    assert_eq!(links, vec![other.span().span_context().clone()]);
    assert_eq!(
        dispatch.span_context.trace_id(),
        same.span().span_context().trace_id(),
        "a link never moves the span to another trace"
    );
}

#[test]
fn traceparent_names_the_span() {
    let _spans = test_spans::capture();
    let span = tracing::info_span!("client");
    let context = span_context(&span);
    assert_eq!(
        traceparent(&span).unwrap(),
        format!("00-{}-{}-01", context.trace_id(), context.span_id())
    );
}
