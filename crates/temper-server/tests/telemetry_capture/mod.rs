//! What the server writes to telemetry during a test, read the way an operator
//! or a telemetry backend would read it: every log event and span field as
//! text, every span the OpenTelemetry SDK exports, and every metric it exports.
//!
//! Installed once per process as the global tracing subscriber and the global
//! OpenTelemetry tracer and meter providers, so it sees spans, events and
//! metrics from every thread and from code that reads the globals directly.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing::field::{Field, Visit};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

/// The installed capture.
pub struct Telemetry {
    text: Arc<Mutex<String>>,
    spans: InMemorySpanExporter,
    tracer_provider: SdkTracerProvider,
    metrics: InMemoryMetricExporter,
    meter_provider: SdkMeterProvider,
}

static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();

/// The capture for this process, installed on first use.
pub fn telemetry() -> &'static Telemetry {
    TELEMETRY.get_or_init(install)
}

fn install() -> Telemetry {
    let text = Arc::new(Mutex::new(String::new()));
    let spans = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(spans.clone())
        .build();
    let metrics = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metrics.clone())
        .build();
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // Everything the kernel's own crates write, at every level; other crates
    // (the WASM compiler, HTTP stacks) at INFO and above.
    let targets = Targets::new()
        .with_target("temper_server", tracing::Level::TRACE)
        .with_target("temper_wasm", tracing::Level::TRACE)
        .with_target("temper_observe", tracing::Level::TRACE)
        .with_target("temper_runtime", tracing::Level::TRACE)
        .with_target("temper_authz", tracing::Level::TRACE)
        .with_target("temper_store_turso", tracing::Level::TRACE)
        .with_target("wasm_guest", tracing::Level::TRACE)
        .with_default(tracing::Level::INFO);
    let subscriber = tracing_subscriber::registry()
        .with(TextLayer(text.clone()))
        .with(
            tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("telemetry-capture")),
        )
        .with(targets);
    tracing::subscriber::set_global_default(subscriber)
        .expect("no other global subscriber in this test binary");

    Telemetry {
        text,
        spans,
        tracer_provider,
        metrics,
        meter_provider,
    }
}

impl Telemetry {
    /// Everything written so far, as one text: log events and span fields,
    /// then the exported spans, then the exported metrics.
    pub fn everything(&self) -> String {
        let mut out = self.text.lock().expect("capture lock").clone();
        let _ = self.tracer_provider.force_flush();
        for span in self.spans.get_finished_spans().unwrap_or_default() {
            let _ = writeln!(out, "exported span: {span:?}");
        }
        let _ = self.meter_provider.force_flush();
        for metrics in self.metrics.get_finished_metrics().unwrap_or_default() {
            let _ = writeln!(out, "exported metrics: {metrics:?}");
        }
        out
    }
}

/// Writes every event and every span field, as it is recorded, into text.
struct TextLayer(Arc<Mutex<String>>);

struct TextVisitor<'a>(&'a mut String);

impl Visit for TextVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={}", field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl<S: tracing::Subscriber> Layer<S> for TextLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = format!("event {}:", event.metadata().target());
        event.record(&mut TextVisitor(&mut line));
        line.push('\n');
        self.0.lock().expect("capture lock").push_str(&line);
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut line = format!("span {}:", attrs.metadata().name());
        attrs.record(&mut TextVisitor(&mut line));
        line.push('\n');
        self.0.lock().expect("capture lock").push_str(&line);
    }

    fn on_record(
        &self,
        _id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: Context<'_, S>,
    ) {
        let mut line = "span record:".to_string();
        values.record(&mut TextVisitor(&mut line));
        line.push('\n');
        self.0.lock().expect("capture lock").push_str(&line);
    }
}
