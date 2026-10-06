//! Renders one run as text, with whatever differs between two runs of the
//! same code masked, so two runs can be compared line by line.

use std::fmt::Write as _;

use crate::child::{
    REDUCED_RATE_DROPPED_TRACE_ID, REDUCED_RATE_KEPT_TRACE_ID, Run, SAMPLED_CALLER_TRACE_ID,
    UNSAMPLED_CALLER_TRACE_ID,
};
use crate::wire::Attributes;

/// Attributes whose values change from run to run, or with how the test
/// harness runs the child (the thread name).
const MASKED_ATTRIBUTES: &[&str] = &[
    "runtime-id",
    "thread.id",
    "thread.name",
    "busy_ns",
    "idle_ns",
    "code.lineno",
];

/// Trace IDs the workload chooses itself; every other trace ID is random.
const CALLER_TRACE_IDS: &[&str] = &[
    UNSAMPLED_CALLER_TRACE_ID,
    SAMPLED_CALLER_TRACE_ID,
    REDUCED_RATE_KEPT_TRACE_ID,
    REDUCED_RATE_DROPPED_TRACE_ID,
];

/// The startup output, the log lines and every exported span, log record and
/// metric.
pub fn render(run: &Run) -> String {
    let mut ids = Ids::default();
    let mut out = String::new();

    out.push_str("== stderr ==\n");
    out.push_str(&run.stderr);

    out.push_str("== log lines ==\n");
    for mut line in run.log_lines() {
        line.remove("timestamp");
        let _ = writeln!(out, "{}", sorted_json(&serde_json::Value::Object(line)));
    }

    out.push_str("== requests ==\n");
    let mut requests: Vec<String> = run
        .received
        .iter()
        .map(|request| format!("{} {}", request.path, request.header("content-type")))
        .collect();
    requests.sort();
    requests.dedup();
    for request in requests {
        let _ = writeln!(out, "{request}");
    }

    out.push_str("== traces ==\n");
    // An export groups its items by scope in no fixed order; within a scope
    // the order is the order of emission.
    let mut spans = run.spans();
    spans.sort_by(|a, b| a.scope.cmp(&b.scope));
    for span in spans {
        let _ = writeln!(
            out,
            "span {:?} scope={} trace={} id={} parent={} kind={} flags={} status={} events={:?}",
            span.name,
            span.scope,
            ids.trace(&span.trace_id),
            ids.span(&span.span_id),
            ids.span(&span.parent_span_id),
            span.kind,
            span.flags,
            span.status_code,
            span.events,
        );
        attributes(&mut out, "resource", &span.resource);
        attributes(&mut out, "attribute", &span.attributes);
    }

    out.push_str("== logs ==\n");
    let mut records = run.logs();
    records.sort_by(|a, b| a.scope.cmp(&b.scope));
    for record in records {
        let time = match record.time_unix_nano {
            0 => "<zero>",
            time if time == record.observed_time_unix_nano => "<observed time>",
            _ => "<other>",
        };
        let observed = match record.observed_time_unix_nano {
            0 => "<zero>",
            _ => "<set>",
        };
        let _ = writeln!(
            out,
            "log {:?} scope={} severity={}({}) time={time} observed={observed} trace={} span={}",
            record.body,
            record.scope,
            record.severity_text,
            record.severity_number,
            ids.trace(&record.trace_id),
            ids.span(&record.span_id),
        );
        attributes(&mut out, "resource", &record.resource);
        attributes(&mut out, "attribute", &record.attributes);
    }

    out.push_str("== metrics ==\n");
    let mut metrics = run.metrics();
    metrics.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
    for metric in metrics {
        let _ = writeln!(
            out,
            "metric {:?} scope={} kind={} unit={:?} description={:?}",
            metric.name, metric.scope, metric.kind, metric.unit, metric.description,
        );
        attributes(&mut out, "resource", &metric.resource);
        let mut points = metric.points;
        points.sort();
        for (point_attributes, value) in points {
            let _ = writeln!(out, "  point {point_attributes:?} = {value}");
        }
    }

    out.replace(&run.endpoint, "http://127.0.0.1:<port>")
}

fn attributes(out: &mut String, label: &str, attributes: &Attributes) {
    for (key, value) in attributes {
        let value = if MASKED_ATTRIBUTES.contains(&key.as_str()) {
            "<masked>"
        } else {
            value.as_str()
        };
        let _ = writeln!(out, "  {label} {key} = {value:?}");
    }
}

/// Compact JSON with the keys of every object sorted, so the text does not
/// depend on whether `serde_json` keeps keys in insertion order, which other
/// crates in a workspace build can switch on.
fn sorted_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(fields) => {
            let mut fields: Vec<(&String, &serde_json::Value)> = fields.iter().collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            let fields: Vec<String> = fields
                .into_iter()
                .map(|(key, value)| {
                    let key = serde_json::Value::from(key.as_str());
                    format!("{key}:{}", sorted_json(value))
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        serde_json::Value::Array(items) => {
            let items: Vec<String> = items.iter().map(sorted_json).collect();
            format!("[{}]", items.join(","))
        }
        scalar => scalar.to_string(),
    }
}

/// Replaces random IDs by labels numbered in order of first appearance.
#[derive(Default)]
struct Ids {
    traces: Vec<String>,
    spans: Vec<String>,
}

impl Ids {
    fn trace(&mut self, id: &str) -> String {
        if id.is_empty() || CALLER_TRACE_IDS.contains(&id) {
            return format!("[{id}]");
        }
        format!("trace#{}", position(&mut self.traces, id))
    }

    fn span(&mut self, id: &str) -> String {
        if id.is_empty() {
            return "[]".to_string();
        }
        format!("span#{}", position(&mut self.spans, id))
    }
}

fn position(seen: &mut Vec<String>, id: &str) -> usize {
    match seen.iter().position(|known| known == id) {
        Some(index) => index + 1,
        None => {
            seen.push(id.to_string());
            seen.len()
        }
    }
}
