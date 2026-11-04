//! Action arguments are redacted in telemetry: a secret-named argument of an
//! action called over HTTP reaches no exported span and no log line, whether
//! the action runs or its guard refuses it, while its other arguments still
//! arrive. Its own test binary, because it installs the process-wide tracer
//! provider and tracing subscriber that the server's telemetry writes through.

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::{ServerState, StorageStack, build_router};
use temper_store_sim::SimEventStore;
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

/// A lock whose actions take a secret-named argument: `Open` runs on a closed
/// lock, and `Rotate` is refused by its guard until the lock has been opened.
const LOCK_IOA: &str = r#"
[automaton]
name = "Lock"
states = ["Closed", "Open"]
initial = "Closed"

[[state]]
name = "opens"
type = "counter"
initial = 0

[[action]]
name = "Open"
kind = "input"
from = ["Closed"]
to = "Open"
params = ["Host", "password"]
effect = ["opens += 1"]

[[action]]
name = "Rotate"
kind = "input"
from = ["Closed"]
guard = "opens > 0"
params = ["Host", "api_token"]
"#;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Lock">
        <Key>
          <PropertyRef Name="Id"/>
        </Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Host" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="Locks" EntityType="Temper.Test.Lock"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

/// Every span the server exports and every log line and span field it writes.
struct Telemetry {
    provider: SdkTracerProvider,
    exporter: InMemorySpanExporter,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Telemetry {
    /// The exported spans' attributes and event attributes, as
    /// `(span name, key, value)`.
    fn span_attributes(&self) -> Vec<(String, String, String)> {
        let _ = self.provider.force_flush();
        let spans = self.exporter.get_finished_spans().expect("finished spans");
        let mut out = Vec::new();
        for span in spans {
            let name = span.name.to_string();
            for kv in &span.attributes {
                out.push((name.clone(), kv.key.to_string(), kv.value.as_str().into()));
            }
            for event in span.events.iter() {
                for kv in &event.attributes {
                    out.push((name.clone(), kv.key.to_string(), kv.value.as_str().into()));
                }
            }
        }
        out
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

/// Writes each event's and span's fields as one text line.
struct Lines(Arc<Mutex<Vec<String>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push_str(&format!(" {}={value}", field.name()));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl<S> tracing_subscriber::Layer<S> for Lines
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = Fields(attrs.metadata().name().to_string());
        attrs.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn on_record(
        &self,
        _: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = Fields(String::from("record"));
        values.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields(event.metadata().target().to_string());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

fn telemetry() -> &'static Telemetry {
    static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();
    TELEMETRY.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());
        let lines = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry()
                .with(Lines(lines.clone()))
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test"))),
        )
        .expect("the only subscriber in this binary");
        Telemetry {
            provider,
            exporter,
            lines,
        }
    })
}

async fn server(name: &str, lock: &str) -> axum::Router {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(CSDL_XML).expect("CSDL parses"),
        CSDL_XML.to_string(),
        &[("Lock", LOCK_IOA)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Lock",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-11-03T00:00:00Z".to_string(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(7), None));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("the policy parses");
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Lock", lock, serde_json::json!({}))
        .await
        .expect("the lock is created");
    build_router(state)
}

async fn act(app: axum::Router, path: &str, body: serde_json::Value) -> (StatusCode, String) {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("redaction-agent", "test-agent", None),
        ));
    let response = app.oneshot(request).await.expect("the router answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("the body reads");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The span named `span` carries `params` with `kept` as sent and `secret_key`
/// as the placeholder, and `secret` reaches no exported span and no log line.
fn assert_redacted(telemetry: &Telemetry, span: &str, kept: &str, secret_key: &str, secret: &str) {
    let attributes = telemetry.span_attributes();
    let params: Vec<&str> = attributes
        .iter()
        .filter(|(name, key, _)| name == span && key == "params")
        .map(|(_, _, value)| value.as_str())
        .collect();
    assert!(
        params.iter().any(|params| params.contains(kept)),
        "the {span} span carries the action's params: {params:?}"
    );
    let leaked: Vec<_> = attributes
        .iter()
        .filter(|(_, _, value)| value.contains(secret))
        .collect();
    assert!(
        leaked.is_empty(),
        "{secret} reached exported spans: {leaked:?}"
    );
    let leaked: Vec<_> = telemetry
        .lines()
        .into_iter()
        .filter(|line| line.contains(secret))
        .collect();
    assert!(leaked.is_empty(), "{secret} reached log lines: {leaked:?}");
    assert!(
        params
            .iter()
            .any(|params| params.contains(&format!("\"{secret_key}\":\"[redacted]\""))),
        "the {span} span names {secret_key} with its value redacted: {params:?}"
    );
}

#[tokio::test]
async fn an_action_that_runs_exports_its_arguments_with_secrets_redacted() {
    let telemetry = telemetry();
    let app = server("telemetry-redaction-runs", "lock-runs").await;

    let (status, body) = act(
        app,
        "/tdata/Locks('lock-runs')/Temper.Open",
        serde_json::json!({
            "Host": "host-runs",
            "password": "SECRET-runs-password",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "Open runs: {body}");

    assert_redacted(
        telemetry,
        "Lock.Open",
        "host-runs",
        "password",
        "SECRET-runs-password",
    );
}

#[tokio::test]
async fn an_action_its_guard_refuses_exports_its_arguments_with_secrets_redacted() {
    let telemetry = telemetry();
    let app = server("telemetry-redaction-refused", "lock-refused").await;

    // Rotate needs an opened lock; the new lock was never opened.
    let (status, body) = act(
        app,
        "/tdata/Locks('lock-refused')/Temper.Rotate",
        serde_json::json!({
            "Host": "host-refused",
            "api_token": "SECRET-refused-token",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "the guard refuses: {body}");

    assert_redacted(
        telemetry,
        "Lock.Rotate",
        "host-refused",
        "api_token",
        "SECRET-refused-token",
    );
}
