//! Who acted, which instance, what kind and for whom are kernel facts: history
//! and a record read say the same, whichever way the move came about.
//!
//! Every call goes through the platform router with a real ES256 token, so the
//! bearer boundary, the resolver and the record reads all run as in production.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const ISSUER: &str = "https://issuer.agent-label.local";
const AUD: &str = "temper-agent-label";
const AGENT: &str = "kc_hub";
const PERSON: &str = "ann";
const KIND: &str = "claude-code";
const LABEL: &str = "hub-session";

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.AgentLabel" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="child" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="parent_type" Type="Edm.String"/>
        <Property Name="parent_id" Type="Edm.String"/>
        <Property Name="job_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Post">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="author" Type="Edm.String"/>
      </EntityType>
      <Action Name="Publish" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Post"/>
      </Action>
      <Action Name="Send" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Post"/>
      </Action>
      <Action Name="Edit" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Post"/>
      </Action>
      <Action Name="Touch" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Job"/>
      </Action>
      <Action Name="Go" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Job"/>
      </Action>
      <Action Name="Arm" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Job"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.AgentLabel.Job"/>
        <EntitySet Name="Children" EntityType="Temper.AgentLabel.Child"/>
        <EntitySet Name="Posts" EntityType="Temper.AgentLabel.Post"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Go` sets off two reactions, the last spawning a Child. `Arm` enters a state
/// whose timer fires `Expire`.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Hopping", "Hopped", "Done", "Waiting", "Expired"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Hopping", "Hopped", "Done", "Expired"]
strict_action_params = true

[[state]]
name = "child"
type = "string"
initial = ""

[[action]]
name = "Touch"
kind = "input"
from = ["Idle"]
params = []

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Hopping"
params = []

[[action.triggers]]
name = "hop"
kind = "entity"
target_entity = "Job"
target_action = "Hop"
resolve_target = { kind = "same_id" }

[[action]]
name = "Hop"
kind = "input"
from = ["Hopping"]
to = "Hopped"
params = []

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Job"
target_action = "Finish"
resolve_target = { kind = "same_id" }

[[action]]
name = "Finish"
kind = "input"
from = ["Hopped"]
to = "Done"
params = []
effect = ["spawn('Child', 'Start', child)"]

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Waiting"
params = []

[[state_timeout]]
state = "Waiting"
after_seconds = AFTER
on_timeout = "Expire"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting"]
to = "Expired"
params = []
"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["Idle", "Started"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Started"]
strict_action_params = true

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Started"
params = ["parent_type", "parent_id", "job_id"]
"#;

/// `Publish` keeps who made the post. `Send` gets there through a reaction,
/// and `Edit` is a later move that leaves it alone.
const POST_IOA: &str = r#"
[automaton]
name = "Post"
states = ["Draft", "Sending", "Posted"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Sending", "Posted"]
strict_action_params = true

[[state]]
name = "author"
type = "string"
initial = ""

[[action]]
name = "Publish"
kind = "input"
from = ["Draft"]
to = "Posted"
params = [{ name = "author", type = "string", source = "authenticated_agent_label" }]

[[action]]
name = "Send"
kind = "input"
from = ["Draft"]
to = "Sending"
params = []

[[action.triggers]]
name = "land"
kind = "entity"
target_entity = "Post"
target_action = "Land"
resolve_target = { kind = "same_id" }

[[action]]
name = "Land"
kind = "input"
from = ["Sending"]
to = "Posted"
params = [{ name = "author", type = "string", source = "authenticated_agent_label" }]

[[action]]
name = "Edit"
kind = "input"
from = ["Posted"]
params = []
"#;

const WITHIN: Duration = Duration::from_secs(20);

fn signing_key() -> SigningKey {
    SigningKey::from_slice(&[13u8; 32]).expect("a P-256 key")
}

fn b64(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
}

fn mint(claims: Value) -> String {
    let header = json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" });
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let signature: Signature = signing_key().sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

fn jwks_json() -> String {
    let point = signing_key().verifying_key().to_encoded_point(false);
    json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
        "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
    }]})
    .to_string()
}

/// The agent acting for ann, with `label` as its `agent_label` claim.
fn agent_labelled(label: &str) -> String {
    mint(json!({
        "iss": ISSUER, "aud": AUD, "sub": PERSON, "client_id": AGENT, "agent_type": KIND,
        "role": "contributor", "agent_label": label, "nbf": 0, "exp": 4_102_444_800i64,
    }))
}

fn agent() -> String {
    agent_labelled(LABEL)
}

/// Ann in person. A person's token that carries a label is still ann in person.
fn person() -> String {
    mint(json!({
        "iss": ISSUER, "aud": AUD, "sub": PERSON, "agent_label": "not-an-agent",
        "nbf": 0, "exp": 4_102_444_800i64,
    }))
}

fn tenant() -> TenantId {
    TenantId::new("default")
}

/// Timer rows committed with the journal's events, as on Turso.
struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// A started server over `store`, under `policy`, with Job's timer at `after`
/// seconds.
async fn server_under(store: &SimEventStore, after: u32, policy: &str) -> PlatformState {
    server_with(store, after, policy, &[]).await
}

/// [`server_under`], with `more` specs besides Job and Child.
async fn server_with(
    store: &SimEventStore,
    after: u32,
    policy: &str,
    more: &[(&str, &str)],
) -> PlatformState {
    let job = JOB_IOA.replace("AFTER", &after.to_string());
    let mut specs = vec![("Job", job.as_str()), ("Child", CHILD_IOA)];
    specs.extend_from_slice(more);
    let mut state = PlatformState::new(None);
    state.server.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal(),
    );
    state.server.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    let cache = BTreeMap::new();
    bootstrap_system_tenant(&state, &cache);
    bootstrap_agent_specs(&state, "default", false, &cache);
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .try_register_tenant_with_constraints(
                tenant(),
                parse_csdl(CSDL).expect("app CSDL parses"),
                CSDL.to_string(),
                &specs,
                None,
                true,
            )
            .expect("the app's specs merge into the tenant");
        for (entity_type, _) in &specs {
            registry.set_verification_status(
                &tenant(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "fixture".to_string(),
                        passed: true,
                        summary: "pre-verified".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-11-02T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
        .server
        .dispatch_tenant_action(
            &tenant(),
            "TrustedIssuer",
            ISSUER,
            "RegisterIssuer",
            json!({
                "issuer": ISSUER, "jwks_json": jwks_json(), "audience": AUD,
                "algorithms": "ES256", "description": "agent label", "created_by": "test",
            }),
            &AgentContext::system(),
        )
        .await
        .expect("register the issuer");
    state
        .server
        .authz
        .reload_tenant_policies("default", policy)
        .expect("the app's Cedar parses");
    state.server.rebuild_reaction_dispatcher();
    state.server.start_runtime().await;
    state
}

async fn server(store: &SimEventStore, after: u32) -> PlatformState {
    server_under(store, after, PERMIT_ALL).await
}

async fn call(state: &PlatformState, request: Request<Body>) -> (StatusCode, Value) {
    let response = build_platform_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

async fn get(state: &PlatformState, uri: &str, token: &str) -> Value {
    let request = Request::get(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(state, request).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {body}");
    body
}

/// Make `action` on a Job, as `token`, creating the record first.
async fn act(state: &PlatformState, id: &str, action: &str, token: &str) {
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Job", id, json!({}))
        .await
        .expect("the record is created");
    let request = Request::post(format!("/tdata/Jobs('{id}')/Temper.AgentLabel.{action}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = call(state, request).await;
    assert!(status.is_success(), "{action} on {id}: {status}: {body}");
}

async fn read(state: &PlatformState, set: &str, id: &str) -> Value {
    get(state, &format!("/tdata/{set}('{id}')"), &agent()).await
}

/// The record's history, oldest first.
async fn history(state: &PlatformState, entity_type: &str, id: &str) -> Vec<Value> {
    let body = get(
        state,
        &format!("/observe/entities/{entity_type}/{id}/history"),
        &agent(),
    )
    .await;
    body["events"]
        .as_array()
        .expect("history has events")
        .clone()
}

/// Wait until the record reaches `status`, or fail saying what it read.
async fn until_status(state: &PlatformState, set: &str, id: &str, status: &str) -> Value {
    let deadline = Instant::now() + WITHIN; // determinism-ok: real-time test
    loop {
        let record = read(state, set, id).await;
        if record["status"] == status {
            return record;
        }
        assert!(
            Instant::now() < deadline, // determinism-ok: real-time test
            "{set} {id} never reached {status}: {record}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

/// A record read names the agent: who, for whom, what kind and which label.
#[track_caller]
fn record_names_the_agent(record: &Value) {
    assert_eq!(record["last_event_by"], AGENT, "{record}");
    assert_eq!(record["last_event_for"], PERSON, "{record}");
    assert_eq!(record["last_event_agent_type"], KIND, "{record}");
    assert_eq!(record["last_event_label"], LABEL, "{record}");
    assert!(record.get("last_event_role").is_none(), "{record}");
}

/// A history row says what a record read says.
#[track_caller]
fn event_names_the_agent(event: &Value) {
    assert_eq!(event["by"], AGENT, "{event}");
    assert_eq!(event["for"], PERSON, "{event}");
    assert_eq!(event["agent_type"], KIND, "{event}");
    assert_eq!(event["label"], LABEL, "{event}");
    assert!(event.get("role").is_none(), "{event}");
}

/// A read returns the agent fields as null, never leaving a reader to guess.
#[track_caller]
fn names_no_agent(value: &Value, fields: [&str; 3]) {
    for field in fields {
        assert_eq!(value.get(field), Some(&Value::Null), "{field}: {value}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_move_names_it_in_history_and_on_every_record_read() {
    let state = server(&SimEventStore::no_faults(1), 3600).await;
    act(&state, "j", "Touch", &agent()).await;

    let record = read(&state, "Jobs", "j").await;
    assert_eq!(record["last_event_via"], "request", "{record}");
    record_names_the_agent(&record);
    event_names_the_agent(history(&state, "Job", "j").await.last().unwrap());

    let listed = get(&state, "/tdata/Jobs", &agent()).await;
    let row = listed["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["entity_id"] == "j")
        .expect("the list holds the record");
    record_names_the_agent(row);

    let selected = get(
        &state,
        "/tdata/Jobs('j')?$select=last_event_for,last_event_agent_type,last_event_label",
        &agent(),
    )
    .await;
    assert_eq!(selected["last_event_for"], PERSON, "{selected}");
    assert_eq!(selected["last_event_agent_type"], KIND, "{selected}");
    assert_eq!(selected["last_event_label"], LABEL, "{selected}");

    for (filter, matches) in [
        (format!("last_event_label eq '{LABEL}'"), true),
        ("last_event_label eq 'someone-else'".to_string(), false),
        (format!("last_event_for eq '{PERSON}'"), true),
        (format!("last_event_agent_type eq '{KIND}'"), true),
    ] {
        let found = get(
            &state,
            &format!("/tdata/Jobs?$filter={}", filter.replace(' ', "%20")),
            &agent(),
        )
        .await;
        let held = found["value"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entity_id"] == "j");
        assert_eq!(held, matches, "{filter}: {found}");
    }
}

/// The label stays down a chain of reactions and onto a record the chain makes.
#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_keeps_the_label_down_the_chain() {
    let state = server(&SimEventStore::no_faults(2), 3600).await;
    act(&state, "j", "Go", &agent()).await;

    let record = until_status(&state, "Jobs", "j", "Done").await;
    assert_eq!(record["last_event_via"], "reaction", "{record}");
    record_names_the_agent(&record);
    let moves = history(&state, "Job", "j").await;
    for action in ["Go", "Hop", "Finish"] {
        let event = moves
            .iter()
            .find(|event| event["action"] == action)
            .unwrap_or_else(|| panic!("{action} is in history: {moves:?}"));
        event_names_the_agent(event);
    }

    let child_id = record["fields"]["child"]
        .as_str()
        .expect("the spawn names its child");
    let child = until_status(&state, "Children", child_id, "Started").await;
    record_names_the_agent(&child);
    event_names_the_agent(history(&state, "Child", child_id).await.last().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_keeps_the_label_of_the_agent_that_set_it() {
    let state = server(&SimEventStore::no_faults(3), 1).await;
    act(&state, "j", "Arm", &agent()).await;

    let record = until_status(&state, "Jobs", "j", "Expired").await;
    assert_eq!(record["last_event_via"], "timer", "{record}");
    record_names_the_agent(&record);
    event_names_the_agent(history(&state, "Job", "j").await.last().unwrap());
}

/// Run `first` on a server of its own, stopped afterward as a process exit
/// would stop it, timers and all.
fn on_a_server_that_stops<F, Fut>(store: &SimEventStore, after: u32, first: F)
where
    F: FnOnce(PlatformState) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let store = store.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let state = server(&store, after).await;
            first(state).await;
        });
        drop(runtime);
    })
    .join()
    .expect("the first server ran");
}

/// The timer row says who set it, so a restart fires as them.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_row_keeps_the_label_across_a_restart() {
    let store = SimEventStore::no_faults(4);
    on_a_server_that_stops(&store, 5, |state| async move {
        act(&state, "j", "Arm", &agent()).await;
    });
    assert!(
        store
            .list_state_timers("default")
            .iter()
            .any(|row| row.entity_id == "j"),
        "the timer left a row"
    );

    let state = server(&store, 5).await;
    let record = until_status(&state, "Jobs", "j", "Expired").await;
    assert_eq!(record["last_event_via"], "timer", "{record}");
    record_names_the_agent(&record);
}

/// With the row gone, the journal says who moved the record, label and all.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_timer_row_gets_the_label_from_the_journal() {
    let store = SimEventStore::no_faults(5);
    on_a_server_that_stops(&store, 3600, |state| async move {
        act(&state, "j", "Arm", &agent()).await;
    });
    let rows = store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == "j")
        .expect("the timer left a row");
    store.delete_state_timer("default", "Job", "j", &row.arm_id);

    let state = server(&store, 1).await;
    let record = until_status(&state, "Jobs", "j", "Expired").await;
    assert_eq!(record["last_event_via"], "timer", "{record}");
    record_names_the_agent(&record);
    event_names_the_agent(history(&state, "Job", "j").await.last().unwrap());
}

/// A person's move names no agent, even after an agent's and even with a label
/// on the person's token.
#[tokio::test(flavor = "multi_thread")]
async fn a_persons_move_shows_no_agent_fields() {
    let state = server(&SimEventStore::no_faults(6), 3600).await;
    act(&state, "j", "Touch", &agent()).await;
    act(&state, "j", "Touch", &person()).await;

    let record = read(&state, "Jobs", "j").await;
    assert_eq!(record["last_event_by"], PERSON, "{record}");
    names_no_agent(
        &record,
        [
            "last_event_for",
            "last_event_agent_type",
            "last_event_label",
        ],
    );
    let moves = history(&state, "Job", "j").await;
    let event = moves.last().unwrap();
    assert_eq!(event["by"], PERSON, "{event}");
    names_no_agent(event, ["for", "agent_type", "label"]);
    event_names_the_agent(&moves[moves.len() - 2]);
}

/// An empty claim is no label, so a reader never gets an empty name.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_label_is_no_label() {
    let state = server(&SimEventStore::no_faults(7), 3600).await;
    act(&state, "j", "Touch", &agent_labelled("")).await;

    let record = read(&state, "Jobs", "j").await;
    assert_eq!(record["last_event_by"], AGENT, "{record}");
    assert_eq!(
        record.get("last_event_label"),
        Some(&Value::Null),
        "{record}"
    );
    let moves = history(&state, "Job", "j").await;
    assert_eq!(moves.last().unwrap().get("label"), Some(&Value::Null));
}

async fn post_server(seed: u64) -> PlatformState {
    server_with(
        &SimEventStore::no_faults(seed),
        3600,
        PERMIT_ALL,
        &[("Post", POST_IOA)],
    )
    .await
}

/// Make `action` on a Post with `body`, as `token`, and say how it answered.
async fn post_move(
    state: &PlatformState,
    id: &str,
    action: &str,
    token: &str,
    body: &str,
) -> (StatusCode, Value) {
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Post", id, json!({}))
        .await
        .expect("the record is created");
    let request = Request::post(format!("/tdata/Posts('{id}')/Temper.AgentLabel.{action}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    call(state, request).await
}

/// A field a move binds keeps who made the record, after later moves change
/// `last_event_label`.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_binds_its_agents_label_and_later_moves_leave_it() {
    let state = post_server(11).await;
    let (status, body) = post_move(&state, "p", "Publish", &agent(), "{}").await;
    assert!(status.is_success(), "{status}: {body}");
    let post = read(&state, "Posts", "p").await;
    assert_eq!(post["fields"]["author"], LABEL, "{post}");

    let (status, body) = post_move(&state, "p", "Edit", &person(), "{}").await;
    assert!(status.is_success(), "{status}: {body}");
    let post = read(&state, "Posts", "p").await;
    assert_eq!(post["fields"]["author"], LABEL, "{post}");
    assert_eq!(post["last_event_by"], PERSON, "{post}");
    assert_eq!(post["last_event_label"], Value::Null, "{post}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_binds_the_label_of_the_agent_that_set_it_going() {
    let state = post_server(12).await;
    let (status, body) = post_move(&state, "p", "Send", &agent(), "{}").await;
    assert!(status.is_success(), "{status}: {body}");
    let post = until_status(&state, "Posts", "p", "Posted").await;
    assert_eq!(post["fields"]["author"], LABEL, "{post}");
}

/// A person in person has no label, even with one on their token, and neither
/// has an agent whose issuer gave it none. The move still lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_with_no_label_binds_it_empty() {
    let state = post_server(13).await;
    for (id, token) in [("in-person", person()), ("unnamed", agent_labelled(""))] {
        let (status, body) = post_move(&state, id, "Publish", &token, "{}").await;
        assert!(status.is_success(), "{id}: {status}: {body}");
        let post = read(&state, "Posts", id).await;
        assert_eq!(post["status"], "Posted", "{post}");
        assert_eq!(post["fields"]["author"], "", "{post}");
    }
}

/// The caller's JSON never supplies it, even when it says the true value.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_cannot_supply_the_label() {
    let state = post_server(14).await;
    for (id, supplied) in [("forged", "someone-else"), ("true", LABEL)] {
        let body = json!({ "author": supplied }).to_string();
        let (status, answer) = post_move(&state, id, "Publish", &agent(), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {answer}");
        assert_eq!(
            answer["error"]["code"], "AuthenticatedParameter",
            "{answer}"
        );
        let post = read(&state, "Posts", id).await;
        assert_eq!(post["status"], "Draft", "{post}");
        assert_eq!(post["fields"]["author"], "", "{post}");
    }
}

/// A move no one makes has no caller to name, as with `authenticated_subject`.
#[tokio::test(flavor = "multi_thread")]
async fn the_kernels_own_identity_cannot_bind_a_label() {
    let state = post_server(15).await;
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Post", "p", json!({}))
        .await
        .expect("the record is created");
    let refused = state
        .server
        .dispatch_tenant_action(
            &tenant(),
            "Post",
            "p",
            "Publish",
            json!({}),
            &AgentContext::system(),
        )
        .await;
    assert!(refused.is_err(), "the kernel names no caller");
    assert_eq!(read(&state, "Posts", "p").await["status"], "Draft");
}

/// A label is a name: Cedar sees neither it nor the claim it came from.
#[tokio::test(flavor = "multi_thread")]
async fn a_label_is_not_an_authorization_input() {
    let policy = r#"
        permit(principal, action, resource);
        forbid(principal, action, resource)
        when {
            principal has label || principal has agent_label ||
            context has label || context has agent_label
        };
    "#;
    let state = server_under(&SimEventStore::no_faults(8), 3600, policy).await;
    act(&state, "j", "Touch", &agent()).await;
    assert_eq!(read(&state, "Jobs", "j").await["last_event_by"], AGENT);
}
