//! DST: a module's answer runs as the identity of the move that called the
//! module, and Cedar decides it as that identity, as a module's answer.
//!
//! A `Job`'s `Ask` (or its timer's `Lapse`) calls a module whose answer is
//! `Answer`, and `Answer` calls a module whose answer is `Confirm`: a module
//! calling a module. `Answer` and `Confirm` bind `owner_id` and `confirmed_by`
//! from the authenticated subject, so the record keeps who each answer ran as.
//! The tenant's Cedar permits the two answers only as `context.via ==
//! "module"`, and only to a verified member.
//!
//! Workload, per seed: records made by each kind of caller at least once, then
//! seeded extras, in a seeded order and at seeded times:
//! - `Person`: a verified member sends `Ask`; its module calls run in the
//!   background;
//! - `Inline`: the same, waiting for its module calls (`?await_integration`);
//! - `Timer`: a verified member sends `Hold`, and the state's timer fires
//!   `Lapse`, which calls the module, as the member who armed it;
//! - `Guest`: a verified caller the policy does not permit the answers;
//! - `Unverified`: an agent whose type is not verified.
//!
//! Invariants, checked once every timer was due and every answer settled:
//! - every answer runs as the calling move's identity: the record is
//!   `Confirmed`, and `owner_id` and `confirmed_by` are the caller's id, never
//!   a service's;
//! - Cedar is asked about each answer as that identity, via a module: a caller
//!   the policy permits gets no would-deny event, and a caller it does not
//!   permit gets exactly one per answer, naming the caller, whether its agent
//!   type is verified, and the trigger that called the module.
//!
//! The server runs in a single-threaded, paused-clock Tokio runtime, and
//! `sim_now()` reads that clock.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use process_clock::ProcessClock;
use serde_json::json;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tracing_subscriber::prelude::*;
use world::SimTimerRows;

const SEEDS: u64 = 12;
const TENANT: &str = "alpha";
/// The `Waiting` timeout in [`JOB`], in seconds.
const TIMER_SECS: u64 = 600;
/// How long after the last move the run goes on: past every timer.
const HORIZON: Duration = Duration::from_secs(2 * TIMER_SECS);

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Asked", "Answered", "Confirmed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Waiting", "Asked", "Answered", "Confirmed"]
# A bound parameter needs strict parameters.
strict_action_params = true

[[state]]
name = "owner_id"
type = "string"
initial = ""

[[state]]
name = "confirmed_by"
type = "string"
initial = ""

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "ask_oracle"
kind = "wasm"
module = "oracle"
on_success = "Answer"

[[action]]
name = "Hold"
kind = "input"
from = ["Idle"]
to = "Waiting"

[[state_timeout]]
state = "Waiting"
after_seconds = 600
on_timeout = "Lapse"

[[action]]
name = "Lapse"
kind = "internal"
from = ["Waiting"]
to = "Asked"

[[action.triggers]]
name = "lapse_oracle"
kind = "wasm"
module = "oracle"
on_success = "Answer"

[[action]]
name = "Answer"
kind = "input"
from = ["Asked"]
to = "Answered"
params = [{ name = "owner_id", type = "string", source = "authenticated_subject" }]

[[action.triggers]]
name = "confirm_oracle"
kind = "wasm"
module = "oracle"
on_success = "Confirm"

[[action]]
name = "Confirm"
kind = "input"
from = ["Answered"]
to = "Confirmed"
params = [{ name = "confirmed_by", type = "string", source = "authenticated_subject" }]
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="owner_id" Type="Edm.String"/>
<Property Name="confirmed_by" Type="Edm.String"/>
</EntityType>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Anyone may make the moves a caller sends. The answers are permitted only
/// as a module's answer, and only to a verified member.
const POLICY: &str = r#"
permit(principal, action in [Action::"Ask", Action::"Hold"], resource);

@id("answers-for-verified-members")
permit(principal, action in [Action::"Answer", Action::"Confirm"], resource)
when {
  context has via && context.via == "module" &&
  principal has agentTypeVerified && principal.agentTypeVerified == true &&
  principal has role && principal.role == "member"
};
"#;

/// A WASM module that answers with no values, whatever it is asked: the
/// trigger's `on_success` sends its answer.
fn oracle_module() -> String {
    let payload = json!({ "params": {} }).to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
            i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    )
}

/// Who calls the module on a record, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    /// A verified member's `Ask`, its module calls in the background.
    Person,
    /// A verified member's `Ask`, waiting for its module calls.
    Inline,
    /// A verified member's `Hold`, whose timer fires `Lapse`.
    Timer,
    /// A verified caller the policy does not permit the answers.
    Guest,
    /// An agent whose type is not verified.
    Unverified,
}

impl Case {
    const ALL: [Case; 5] = [
        Case::Person,
        Case::Inline,
        Case::Timer,
        Case::Guest,
        Case::Unverified,
    ];

    /// The move the caller sends.
    fn sends(self) -> &'static str {
        match self {
            Self::Timer => "Hold",
            _ => "Ask",
        }
    }

    /// The trigger whose module answers with `Answer`.
    fn first_rule(self) -> &'static str {
        match self {
            Self::Timer => "Job:Lapse:lapse_oracle",
            _ => "Job:Ask:ask_oracle",
        }
    }

    /// Whether the policy permits this caller the answers.
    fn permitted(self) -> bool {
        !matches!(self, Self::Guest | Self::Unverified)
    }

    /// The caller of record `i`, by its principal id.
    fn caller(self, id: &str) -> AgentContext {
        let security_ctx = match self {
            Self::Person | Self::Inline | Self::Timer => SecurityContext::from_verified_jwt(
                id,
                PrincipalKind::Agent,
                Some("helper"),
                None,
                Some("member"),
                None,
            ),
            Self::Guest => SecurityContext::from_verified_jwt(
                id,
                PrincipalKind::Agent,
                Some("helper"),
                None,
                Some("guest"),
                None,
            ),
            Self::Unverified => {
                SecurityContext::anonymous().with_agent_context(Some(id), None, Some("helper"))
            }
        };
        AgentContext {
            security_ctx: Some(security_ctx),
            ..Default::default()
        }
    }
}

/// One would-deny event: the record, the answer, and who and what it named.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WouldDeny {
    entity_id: String,
    action: String,
    principal_id: String,
    agent_type_verified: String,
    rule: String,
}

/// The would-deny events of a run, in the order they came.
#[derive(Clone, Default)]
struct Denials(Arc<Mutex<Vec<WouldDeny>>>);

impl Denials {
    fn of(&self, entity_id: &str) -> Vec<WouldDeny> {
        let events = self.0.lock().unwrap();
        events
            .iter()
            .filter(|event| event.entity_id == entity_id)
            .cloned()
            .collect()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// The fields of an event, by name, and its `evt`.
#[derive(Default)]
struct EventFields(BTreeMap<String, String>);

impl tracing::field::Visit for EventFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Denials {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = EventFields::default();
        event.record(&mut fields);
        if fields.0.get("evt").map(String::as_str) != Some("module_answer_would_deny") {
            return;
        }
        let field = |name: &str| fields.0.get(name).cloned().unwrap_or_default();
        self.0.lock().unwrap().push(WouldDeny {
            entity_id: field("entity_id"),
            action: field("action"),
            principal_id: field("principal_id"),
            agent_type_verified: field("agent_type_verified"),
            rule: field("rule"),
        });
    }
}

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let `by` pass. The paused clock jumps from one timer to the next.
async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
    settle().await;
}

fn fail(seed: u64, trace: &[String], invariant: &str, detail: String) -> String {
    format!(
        "seed {seed}: {invariant}: {detail}\ntrace:\n  {}",
        trace.join("\n  ")
    )
}

/// One seed's run.
fn run_seed(seed: u64, denials: &Denials) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let events = SimEventStore::no_faults(seed);
    let rows = Arc::new(SimTimerRows::new(seed));
    let mut rng = DeterministicRng::new(seed ^ 0x0a_11_5e);
    for _ in 0..8 {
        rng.next_u64();
    }
    let mut trace = Vec::new();
    denials.clear();

    // Every case at least once, then seeded extras, in a seeded order.
    let mut cases = Case::ALL.to_vec();
    for _ in 0..rng.next_bound(5) {
        cases.push(Case::ALL[rng.next_bound(Case::ALL.len())]);
    }
    for i in (1..cases.len()).rev() {
        cases.swap(i, rng.next_bound(i + 1));
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        clock.begin();
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            TENANT,
            parse_csdl(CSDL).unwrap(),
            CSDL.to_string(),
            &[("Job", JOB)],
        );
        let mut state =
            ServerState::from_registry(ActorSystem::new(format!("answers-{seed}")), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        state.state_timer_rows = Some(rows.clone());
        state
            .authz
            .reload_tenant_policies(TENANT, POLICY)
            .expect("the tenant policy parses");
        state.rebuild_reaction_dispatcher();
        let tenant = TenantId::new(TENANT);
        let hash = state
            .wasm_engine
            .compile_and_cache(oracle_module().as_bytes())
            .expect("the module compiles");
        state.register_wasm_module(&tenant, "oracle", &hash);
        let walk = state.start_runtime().await.expect("the server starts");
        walk.await.unwrap();

        let mut records: Vec<(String, Case)> = Vec::new();
        for (i, case) in cases.iter().copied().enumerate() {
            let id = format!("job-{i}");
            let principal = format!("caller-{i}");
            pass(Duration::from_secs(rng.next_bound(300) as u64)).await;
            state
                .get_or_create_tenant_entity(&tenant, "Job", &id, json!({}))
                .await
                .map_err(|error| fail(seed, &trace, "records are created", error.to_string()))?;
            let caller = case.caller(&principal);
            let options = DispatchExtOptions {
                agent_ctx: &caller,
                await_integration: case == Case::Inline,
                await_reactions: true,
            };
            let sent = state
                .dispatch_tenant_action_ext(&tenant, "Job", &id, case.sends(), json!({}), options)
                .await;
            let answer = match &sent {
                Ok(response) => format!("success {}, error {:?}", response.success, response.error),
                Err(error) => format!("dispatch error {error}"),
            };
            trace.push(format!(
                "{id}: {case:?} as {principal}, {} at {}: {answer}",
                case.sends(),
                sim_now()
            ));
            records.push((id, case));
        }
        pass(HORIZON).await;
        settle().await;

        for (i, (id, case)) in records.iter().enumerate() {
            let principal = format!("caller-{i}");
            let record = state
                .get_tenant_entity_state(&tenant, "Job", id)
                .await
                .map_err(|error| fail(seed, &trace, "records are readable", error.to_string()))?;
            let field = |name: &str| {
                record
                    .state
                    .fields
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let (owner, confirmer) = (field("owner_id"), field("confirmed_by"));
            let seen = denials.of(id);
            let detail = format!(
                "{id} {case:?} as {principal}: status {}, owner_id '{owner}', \
                 confirmed_by '{confirmer}', would-deny events {seen:?}",
                record.state.status
            );
            if record.state.status != "Confirmed" || owner != principal || confirmer != principal {
                return Err(fail(
                    seed,
                    &trace,
                    "every answer runs as the calling move's identity",
                    detail,
                ));
            }
            let verified = if *case == Case::Unverified {
                "false"
            } else {
                "true"
            };
            let expected: Vec<WouldDeny> = if case.permitted() {
                Vec::new()
            } else {
                vec![
                    WouldDeny {
                        entity_id: id.clone(),
                        action: "Answer".to_string(),
                        principal_id: principal.clone(),
                        agent_type_verified: verified.to_string(),
                        rule: case.first_rule().to_string(),
                    },
                    WouldDeny {
                        entity_id: id.clone(),
                        action: "Confirm".to_string(),
                        principal_id: principal.clone(),
                        agent_type_verified: verified.to_string(),
                        rule: "Job:Answer:confirm_oracle".to_string(),
                    },
                ]
            };
            if seen != expected {
                return Err(fail(
                    seed,
                    &trace,
                    "Cedar decides each answer as the caller, via a module",
                    detail,
                ));
            }
        }
        Ok(())
    });
    runtime.block_on(async { clock.end(Duration::ZERO) });
    drop(runtime);
    result
}

#[test]
fn dst_a_modules_answer_runs_as_the_calling_moves_identity() {
    let denials = Denials::default();
    let _logs =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(denials.clone()));
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed, &denials) {
            let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
            let entry = broken
                .entry(invariant)
                .or_insert_with(|| (Vec::new(), report));
            entry.0.push(seed);
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}
