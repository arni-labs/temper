//! Each integration's callback has an idempotency key of its own.
//!
//! Two integrations on one action can answer with the same callback action and
//! the same parameters. Under a caller's `Idempotency-Key` both callbacks still
//! apply, and a retry of the request applies neither again.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SERVICE: &str = "sibling-callback-test";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Siblings" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Survey">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Surveys" EntityType="Temper.Siblings.Survey"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Ask` calls two outside services; each answers with `Answered` and the same
/// result, and every `Answered` counts.
const SURVEY_IOA: &str = r#"
[automaton]
name = "Survey"
states = ["Idle", "Asking"]
initial = "Idle"

[[state]]
name = "answers"
type = "counter"
initial = 0

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asking"

[[action.triggers]]
name = "ask_first"
kind = "adapter"
adapter = "http"
on_success = "Answered"
on_failure = "Unanswered"
config = { method = "POST", url = "{url}/ask" }

[[action.triggers]]
name = "ask_second"
kind = "adapter"
adapter = "http"
on_success = "Answered"
on_failure = "Unanswered"
config = { method = "POST", url = "{url}/ask" }

[[action]]
name = "Answered"
kind = "input"
from = ["Asking"]
params = ["result"]
effect = ["answers += 1"]

[[action]]
name = "Unanswered"
kind = "input"
from = ["Asking"]
params = ["error_message"]
"#;

fn build_state(tenant: &str, survey_ioa: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Survey", survey_ioa)],
        )
        .expect("tenant registration");
    let state = ServerState::from_registry(ActorSystem::new("sibling-callbacks"), registry);
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal == Agent::"service:sibling-callback-test", action, resource);"#,
        )
        .expect("policy should load");
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn two_integrations_answering_alike_both_apply_under_one_caller_key() {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ask"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "callback_params": { "result": "yes" } })),
        )
        .mount(&outside)
        .await;
    let tenant = TenantId::new("surveys");
    let state = build_state("surveys", &SURVEY_IOA.replace("{url}", &outside.uri()));
    let who = AgentContext {
        idempotency_key: Some("ask-1".to_string()),
        ..AgentContext::for_service(SERVICE)
    };
    let ask = || {
        state.dispatch_tenant_action_ext(
            &tenant,
            "Survey",
            "s1",
            "Ask",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &who,
                await_integration: true,
                await_reactions: true,
            },
        )
    };
    let answers = || async {
        let entity = state
            .get_tenant_entity_state(&tenant, "Survey", "s1")
            .await
            .expect("record");
        entity.state.counters.get("answers").copied().unwrap_or(0)
    };

    let first = ask().await.expect("Ask dispatches");
    assert!(first.success, "{:?}", first.error);
    assert_eq!(
        answers().await,
        2,
        "each integration's callback applied, though both sent the same answer"
    );

    let retry = ask().await.expect("retried Ask dispatches");
    assert!(retry.success, "{:?}", retry.error);
    assert_eq!(answers().await, 2, "a retry applies neither callback again");
}
