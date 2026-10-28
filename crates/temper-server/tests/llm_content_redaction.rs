//! A WASM module's LLM content stays out of telemetry for a tenant that has not
//! opted into content export, and out of the record for every tenant.
//!
//! A real module, run by the real engine on a real action, writes prompt and
//! completion text into every channel a guest has: its own span (start,
//! attributes, event, end), a metric tag, a wide event's tags and attributes,
//! and the `_gen_ai_*` params of its result. The test reads what an operator
//! and a telemetry backend would see: every log line and span field the server
//! wrote, every span and metric the OpenTelemetry SDK exported, and the record
//! over OData.
//!
//! The same module also runs for a tenant that opted in, and its text must show
//! up in telemetry: that proves the capture sees each channel, so an absence
//! for the other tenant means the text was redacted, not that nothing looked.

mod telemetry_capture;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_wasm::host_trait::MAX_REDACTED_LLM_METADATA_VALUE_BYTES;
use tower::ServiceExt;

/// Has not opted into LLM content export: the default for every tenant.
const REDACTED: &str = "llm-content-redacted";
/// Opted in through `TEMPER_LLM_CONTENT_EXPORT_TENANTS`.
const OPTED_IN: &str = "llm-content-opted-in";

const CALL_IOA: &str = r#"
[automaton]
name = "Call"
states = ["Idle", "Asked", "Answered"]
initial = "Idle"

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"
hint = "Asks the model."

[[action.triggers]]
name = "model_call"
kind = "wasm"
module = "model_call"
llm = true
on_success = "Answer"

[[action]]
name = "Answer"
kind = "input"
from = ["Asked"]
to = "Answered"
params = ["output", { name = "input_tokens", type = "int" }]
hint = "The model's answer."
"#;

const CALL_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Call" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Call">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Calls" EntityType="Temper.Call.Call"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A model name far past the metadata bound, so a redacted value must be cut.
fn long_model(tenant: &str) -> String {
    format!(
        "model-of-{tenant}-{}",
        "Q".repeat(4 * MAX_REDACTED_LLM_METADATA_VALUE_BYTES)
    )
}

/// A model name inside the bound, which a redacted tenant keeps whole.
fn short_model(tenant: &str) -> String {
    format!("claude-for-{tenant}")
}

/// Where each piece of content goes, and the text it carries for `tenant`.
/// Every text is unique to its channel and tenant, so a leak names both.
struct Channel {
    name: &'static str,
    text: String,
    /// Whether an opted-in tenant's content reaches telemetry through it.
    /// `_gen_ai_completion` and `_dd_llmobs_tool_spans` reach no sink for
    /// this module even when opted in; they still must not reach the record.
    exported_when_opted_in: bool,
}

fn channels(tenant: &str) -> Vec<Channel> {
    let channel = |name: &'static str, exported_when_opted_in: bool| Channel {
        name,
        text: format!("SECRET-{name}-{tenant}"),
        exported_when_opted_in,
    };
    vec![
        channel("span-start", true),
        channel("span-attributes", true),
        channel("span-event", true),
        channel("span-end", true),
        channel("metric-tag", true),
        channel("wide-event-tag", true),
        channel("wide-event-attribute", true),
        channel("result-input-messages", true),
        channel("result-output-messages", true),
        channel("result-system-instructions", true),
        channel("result-completion", false),
        channel("result-tool-spans", false),
    ]
}

fn text_of<'a>(channels: &'a [Channel], name: &str) -> &'a str {
    &channels
        .iter()
        .find(|channel| channel.name == name)
        .expect("a known channel")
        .text
}

/// The guest's calls, in order: each host function and the JSON it is given.
/// The module's own application telemetry, outside the `gen_ai.*` namespace:
/// kept for every tenant.
fn diagnostic(tenant: &str) -> String {
    format!("diagnostic-step-of-{tenant}")
}

fn guest_calls(tenant: &str) -> Vec<(&'static str, String)> {
    let c = channels(tenant);
    let t = |name: &str| text_of(&c, name).to_string();
    let model = long_model(tenant);
    let short = short_model(tenant);
    vec![
        (
            "start_span",
            serde_json::json!({
                "name": "guest.model_call",
                "attributes": {
                    "gen_ai.input.messages": t("span-start"),
                    "gen_ai.request.model": model,
                    "app.step": diagnostic(tenant),
                },
            })
            .to_string(),
        ),
        (
            "set_span_attributes",
            serde_json::json!({ "attributes": { "gen_ai.completion": t("span-attributes") } })
                .to_string(),
        ),
        (
            "add_span_event",
            serde_json::json!({
                "name": "guest.chunk",
                "attributes": { "gen_ai.prompt": t("span-event") },
            })
            .to_string(),
        ),
        (
            "end_span",
            serde_json::json!({
                "status": "ok",
                "attributes": { "gen_ai.output.messages": t("span-end") },
            })
            .to_string(),
        ),
        (
            "emit_metric",
            serde_json::json!({
                "name": "guest_model_calls",
                "value": 1.0,
                "kind": "counter",
                "tags": {
                    "gen_ai.prompt": t("metric-tag"),
                    "gen_ai.request.model": short,
                },
            })
            .to_string(),
        ),
        (
            "emit_wide_event",
            serde_json::json!({
                "kind": "LlmCall",
                "operation": "chat",
                "tags": { "gen_ai.input.messages": t("wide-event-tag") },
                "attributes": {
                    "gen_ai.completion": t("wide-event-attribute"),
                    "gen_ai.request.model": short,
                },
                "measurements": { "gen_ai.usage.input_tokens": 10.0 },
            })
            .to_string(),
        ),
        (
            "set_result",
            serde_json::json!({
                "action": "Answer",
                "success": true,
                "params": {
                    "output": "ordinary output",
                    "input_tokens": 10,
                    "_gen_ai_input_messages": t("result-input-messages"),
                    "_gen_ai_output_messages": t("result-output-messages"),
                    "_gen_ai_system_instructions": t("result-system-instructions"),
                    "_gen_ai_completion": t("result-completion"),
                    "_dd_llmobs_tool_spans": format!("[{{\"arguments\":\"{}\"}}]", t("result-tool-spans")),
                    "_gen_ai_provider": "anthropic",
                    "_gen_ai_model": model,
                },
            })
            .to_string(),
        ),
    ]
}

/// A WAT module that makes `calls` in order, each payload laid out in memory.
fn guest_module(calls: &[(&'static str, String)]) -> String {
    let mut data = String::new();
    let mut body = String::new();
    let mut offset = 0usize;
    for (call, payload) in calls {
        let escaped: String = payload.bytes().map(|b| format!("\\{b:02x}")).collect();
        data.push_str(&format!("(data (i32.const {offset}) \"{escaped}\")\n"));
        let (ptr, len) = (offset, payload.len());
        body.push_str(&match *call {
            "start_span" => {
                format!("i32.const {ptr} i32.const {len} call $start_span local.set $span\n")
            }
            "set_span_attributes" | "add_span_event" | "end_span" => {
                format!("local.get $span i32.const {ptr} i32.const {len} call ${call} drop\n")
            }
            "emit_metric" | "emit_wide_event" => {
                format!("i32.const {ptr} i32.const {len} call ${call} drop\n")
            }
            "set_result" => format!("i32.const {ptr} i32.const {len} call $set_result\n"),
            other => panic!("no host call {other}"),
        });
        offset += payload.len() + 16;
    }
    assert!(offset < 65_536, "payloads fit the module's one page");
    format!(
        r#"(module
  (import "env" "host_start_span" (func $start_span (param i32 i32) (result i64)))
  (import "env" "host_set_span_attributes" (func $set_span_attributes (param i64 i32 i32) (result i32)))
  (import "env" "host_add_span_event" (func $add_span_event (param i64 i32 i32) (result i32)))
  (import "env" "host_end_span" (func $end_span (param i64 i32 i32) (result i32)))
  (import "env" "host_emit_metric" (func $emit_metric (param i32 i32) (result i32)))
  (import "env" "host_emit_wide_event" (func $emit_wide_event (param i32 i32) (result i32)))
  (import "env" "host_set_result" (func $set_result (param i32 i32)))
  (memory (export "memory") 1)
  {data}
  (func (export "run") (param i32 i32) (result i32)
    (local $span i64)
    {body}
    i32.const 0))"#
    )
}

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    for tenant in [REDACTED, OPTED_IN] {
        registry.register_tenant(
            tenant,
            parse_csdl(CALL_CSDL_XML).expect("CSDL parses"),
            CALL_CSDL_XML.to_string(),
            &[("Call", CALL_IOA)],
        );
    }
    let state = ServerState::from_registry(ActorSystem::new("llm-content-redaction"), registry);
    for tenant in [REDACTED, OPTED_IN] {
        state
            .authz
            .reload_tenant_policies(tenant, "permit(principal, action, resource);")
            .expect("the test policy parses");
        let hash = state
            .wasm_engine
            .compile_and_cache(guest_module(&guest_calls(tenant)).as_bytes())
            .expect("the guest module compiles");
        state.register_wasm_module(&TenantId::new(tenant), "model_call", &hash);
    }
    state
}

/// The record as a reader gets it over OData.
async fn read_call(state: &ServerState, tenant: &str) -> (StatusCode, String) {
    let mut request = Request::get("/tdata/Calls('call-1')")
        .body(Body::empty())
        .expect("the request builds");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("llm-redaction-reader", "test-agent", None),
        ));
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("the body reads");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Ask, then wait for the module's answer to move the record.
async fn ask_and_wait(state: &ServerState, tenant: &str) -> String {
    let asked = state
        .dispatch_tenant_action(
            &TenantId::new(tenant),
            "Call",
            "call-1",
            "Ask",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("Ask dispatches");
    assert!(asked.success, "{tenant}: Ask is taken: {:?}", asked.error);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = read_call(state, tenant).await;
        assert_eq!(status, StatusCode::OK, "{tenant}: the record reads: {body}");
        let record: serde_json::Value = serde_json::from_str(&body).expect("the record is JSON");
        if record["status"] == "Answered" {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{tenant}: the module's answer never moved the record: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn llm_content_from_a_module_is_redacted_unless_the_tenant_opted_in() {
    // SAFETY: set before any thread reads the environment; this binary holds
    // one test.
    unsafe { std::env::set_var("TEMPER_LLM_CONTENT_EXPORT_TENANTS", OPTED_IN) };
    let telemetry = telemetry_capture::telemetry();
    let state = build_state();

    let records = [
        (REDACTED, ask_and_wait(&state, REDACTED).await),
        (OPTED_IN, ask_and_wait(&state, OPTED_IN).await),
    ];

    // The opted-in tenant's content reaches telemetry through each channel,
    // which shows the capture reads every one. Spans export as they end, so
    // give the last of them a moment.
    let opted_in = channels(OPTED_IN);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut seen;
    loop {
        seen = telemetry.everything();
        let missing: Vec<&str> = opted_in
            .iter()
            .filter(|channel| channel.exported_when_opted_in && !seen.contains(&channel.text))
            .map(|channel| channel.name)
            .collect();
        if missing.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "an opted-in tenant's content must reach telemetry; these channels showed none \
             of it, so the capture cannot vouch for them: {missing:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The other tenant's content reaches no log line, span or metric.
    let leaked: Vec<&str> = channels(REDACTED)
        .iter()
        .filter(|channel| seen.contains(&channel.text))
        .map(|channel| channel.name)
        .collect();
    assert!(
        leaked.is_empty(),
        "a tenant that has not opted in had its LLM content written to telemetry \
         through: {leaked:?}"
    );
    // Its own diagnostics still arrive, and so does its metadata: a short
    // model name whole, a long one cut to the bound, so a prompt cannot ride
    // in under a metadata name.
    assert!(
        seen.contains(&diagnostic(REDACTED)),
        "a module's own span attributes must still reach telemetry for a redacted tenant"
    );
    assert!(
        seen.contains(&short_model(REDACTED)),
        "LLM metadata must still reach telemetry for a redacted tenant"
    );
    let long = long_model(REDACTED);
    assert!(
        seen.contains(&long[..64]),
        "a long model name is cut, not dropped, for a redacted tenant"
    );
    assert!(
        !seen.contains(&long[..MAX_REDACTED_LLM_METADATA_VALUE_BYTES + 1]),
        "a redacted tenant's metadata must be cut to {MAX_REDACTED_LLM_METADATA_VALUE_BYTES} bytes"
    );

    // Neither tenant's record keeps any `_gen_ai_*` param, whatever it opted into.
    for (tenant, record) in &records {
        assert!(
            record.contains("ordinary output"),
            "{tenant}: the module's ordinary output reaches the record: {record}"
        );
        let in_record: Vec<&str> = channels(tenant)
            .iter()
            .filter(|channel| record.contains(&channel.text))
            .map(|channel| channel.name)
            .collect();
        assert!(
            in_record.is_empty() && !record.contains("_gen_ai_") && !record.contains("_dd_llmobs_"),
            "{tenant}: LLM observability params reached the record ({in_record:?}): {record}"
        );
    }
}
