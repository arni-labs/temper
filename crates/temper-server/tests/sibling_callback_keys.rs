//! Each integration's callback has an idempotency key of its own.
//!
//! Two integrations on one action can answer with the same callback action and
//! the same parameters. Under a caller's `Idempotency-Key` both callbacks still
//! apply, and a retry of the request applies neither again, whether they run
//! inline or in the background, even when the outside service answers the
//! retry differently.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::idempotency::IdempotencyKey;
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
        idempotency_key: Some(IdempotencyKey::Request("ask-1".to_string())),
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

/// [`SURVEY_IOA`] calling `/first` and `/second`. The second has no failure
/// callback, so its failure fails the request; every `Unanswered` counts in
/// `misses`, so one applied beside an `Answered` shows.
const TWO_SERVICES_IOA: &str = r#"
[automaton]
name = "Survey"
states = ["Idle", "Asking"]
initial = "Idle"

[[state]]
name = "answers"
type = "counter"
initial = 0

[[state]]
name = "misses"
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
config = { method = "POST", url = "{url}/first" }

[[action.triggers]]
name = "ask_second"
kind = "adapter"
adapter = "http"
on_success = "Answered"
config = { method = "POST", url = "{url}/second" }

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
effect = ["misses += 1"]
"#;

/// Survey `s1` as one caller sees it, asking under one `Idempotency-Key`.
struct Survey {
    state: ServerState,
    tenant: TenantId,
    who: AgentContext,
}

impl Survey {
    /// A survey on [`TWO_SERVICES_IOA`] calling `outside`, asked under `key`.
    fn new(outside: &MockServer, key: &str) -> Self {
        Self {
            state: build_state(
                "surveys",
                &TWO_SERVICES_IOA.replace("{url}", &outside.uri()),
            ),
            tenant: TenantId::new("surveys"),
            who: AgentContext {
                idempotency_key: Some(IdempotencyKey::Request(key.to_string())),
                ..AgentContext::for_service(SERVICE)
            },
        }
    }

    /// `Ask`, answered once its integrations and their callbacks have run.
    async fn ask_inline(&self) -> (bool, Option<String>) {
        self.ask(true).await
    }

    /// `Ask`, answered before its integrations run in the background.
    async fn ask_in_background(&self) -> (bool, Option<String>) {
        self.ask(false).await
    }

    async fn ask(&self, await_integration: bool) -> (bool, Option<String>) {
        let response = self
            .state
            .dispatch_tenant_action_ext(
                &self.tenant,
                "Survey",
                "s1",
                "Ask",
                serde_json::json!({}),
                DispatchExtOptions {
                    agent_ctx: &self.who,
                    await_integration,
                    await_reactions: true,
                },
            )
            .await
            .expect("Ask dispatches");
        (response.success, response.error)
    }

    /// The survey's counter `name`.
    async fn count(&self, name: &str) -> usize {
        let entity = self
            .state
            .get_tenant_entity_state(&self.tenant, "Survey", "s1")
            .await
            .expect("record");
        entity.state.counters.get(name).copied().unwrap_or(0)
    }

    /// Whether `answers` reaches `target` within `limit`, polling every 20 ms.
    async fn answers_reach(&self, target: usize, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < deadline {
            if self.count("answers").await == target {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

/// An outside service where `/first` answers `yes` once and `then` after,
/// and `/second` fails its first call and answers `yes` after. A request's
/// first run commits `/first`'s callback and then fails on `/second`, which
/// leaves its integrations to the retry.
async fn failing_partway(then: ResponseTemplate) -> MockServer {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(answer("yes"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(then)
        .with_priority(2)
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(answer("yes"))
        .with_priority(2)
        .mount(&outside)
        .await;
    outside
}

fn answer(result: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({ "callback_params": { "result": result } }))
}

/// How many calls the outside service has had on `route`.
async fn calls(outside: &MockServer, route: &str) -> usize {
    outside
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == route)
        .count()
}

/// A callback that runs in the background, after the request has answered,
/// keeps its key too. The first request commits `/first`'s callback and fails
/// on `/second`; its retry runs both again in the background, where `/first`
/// now fails. That failure callback is answered by the committed success.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_applies_no_background_callback_again() {
    let outside = failing_partway(ResponseTemplate::new(500)).await;
    let survey = Survey::new(&outside, "ask-4");

    let (success, _) = survey.ask_inline().await;
    assert!(!success, "the second integration failed the request");
    assert_eq!(
        survey.count("answers").await,
        1,
        "the first callback committed"
    );

    let (success, error) = survey.ask_in_background().await;
    assert!(success, "{error:?}");
    // One task runs the integrations in order and waits for each callback, so
    // `/first`'s callback has landed once `/second`'s has.
    assert!(
        survey.answers_reach(2, Duration::from_secs(10)).await,
        "the retry's second callback applies"
    );
    assert_eq!(
        calls(&outside, "/first").await,
        2,
        "the retry ran the first integration again"
    );
    assert_eq!(
        survey.count("misses").await,
        0,
        "a retry applied a background callback beside the committed one"
    );
}

/// A retry whose outside service answers it differently does not apply the
/// committed callback again: the first request commits `/first`'s `yes` and
/// fails on `/second`, and the retry's `/first` answers `changed`.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_request_that_failed_partway_does_not_apply_a_committed_callback_again() {
    let outside = failing_partway(answer("changed")).await;
    let survey = Survey::new(&outside, "ask-2");

    let (success, _) = survey.ask_inline().await;
    assert!(!success, "the second integration failed the request");
    assert_eq!(
        survey.count("answers").await,
        1,
        "the first callback committed"
    );

    let (success, error) = survey.ask_inline().await;
    assert!(success, "{error:?}");
    assert_eq!(
        calls(&outside, "/first").await,
        2,
        "the retry ran the first integration again"
    );
    assert_eq!(
        survey.count("answers").await,
        2,
        "the committed callback is not applied again; the second applies once"
    );
}

/// An integration whose success callback committed, and which fails when a
/// retry of its request runs it again, keeps the outcome it had: its failure
/// callback is answered by the committed success, not applied beside it.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_whose_integration_now_fails_keeps_its_committed_outcome() {
    let outside = failing_partway(ResponseTemplate::new(500)).await;
    let survey = Survey::new(&outside, "ask-3");

    let (success, _) = survey.ask_inline().await;
    assert!(!success, "the second integration failed the request");
    assert_eq!(
        survey.count("answers").await,
        1,
        "the first callback committed"
    );

    let (success, error) = survey.ask_inline().await;
    assert!(success, "{error:?}");
    assert_eq!(
        calls(&outside, "/first").await,
        2,
        "the retry ran the first integration again, and it failed"
    );
    assert_eq!(
        survey.count("misses").await,
        0,
        "the failure callback was applied beside the committed success"
    );
    assert_eq!(
        survey.count("answers").await,
        2,
        "the second integration answered once"
    );
}

/// A caller that hangs up does not end its request's integrations: they run
/// on. Its retry waits for them, as long as the outside service takes, and
/// answers with their outcome; it runs none of them again.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_hang_up_waits_for_the_integrations_still_running() {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(answer("yes"))
        .mount(&outside)
        .await;
    // The second service takes 2 s to answer, so the caller hangs up first.
    let hold = Duration::from_secs(2);
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(answer("yes").set_delay(hold))
        .mount(&outside)
        .await;
    let survey = std::sync::Arc::new(Survey::new(&outside, "ask-5"));

    let hung_up = tokio::spawn({
        let survey = survey.clone();
        async move { survey.ask_inline().await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while calls(&outside, "/second").await == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second outside call never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let held_from = tokio::time::Instant::now();
    assert_eq!(
        survey.count("answers").await,
        1,
        "the first callback committed"
    );
    hung_up.abort();
    assert!(hung_up.await.expect_err("aborted").is_cancelled());

    let (success, error) = survey.ask_inline().await;
    assert!(success, "{error:?}");
    assert_eq!(
        survey.count("answers").await,
        2,
        "the retry answered once the second callback had applied"
    );
    assert_eq!(
        (
            calls(&outside, "/first").await,
            calls(&outside, "/second").await
        ),
        (1, 1),
        "the retry ran no integration again"
    );
    assert!(
        held_from.elapsed() < hold + Duration::from_secs(5),
        "the retry waited for the held call, not a timeout: {:?}",
        held_from.elapsed()
    );
}
