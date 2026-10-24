//! DST: a parent never keeps the id of a child the kernel could not create.
//!
//! A `spawn(...)` effect writes the child's id into the parent's field as part
//! of the parent's transition, and the kernel creates the child after that
//! transition commits (ADR-0015). The child's create can still be refused: its
//! initial action's parameters fail their contract, or the store fails while
//! the child is created. The workload is a scheduled job that fires runs, like
//! a CronJob spawning Sessions: each fire spawns a `Run` whose `Configure`
//! requires a nonempty `user_message`. Seeded choices give some fires a
//! message, some an empty one, and some a store that fails reads of the new
//! child.
//!
//! Invariants, checked once every spawn has settled:
//! 1. a parent's spawn field names a child that exists, or is empty;
//! 2. every child that was not created is named by a `SpawnRefused` event in
//!    its parent's journal, so the refusal is visible after the fact.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{DeterministicRng, SimEventStore};

const SEEDS: u64 = 32;

/// How long one spawn may take to settle. A settled spawn has a child or a
/// refusal on its parent; the background task needs a few actor round trips.
const SETTLE_POLLS: usize = 100;
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.SpawnChild" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="last_run_id" Type="Edm.String"/>
        <Property Name="user_message" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="user_message" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.SpawnChild.Job"/>
        <EntitySet Name="Runs" EntityType="Temper.SpawnChild.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A scheduled job: each `Fire` spawns a `Run` and stores its id.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Active"]
initial = "Active"

[[state]]
name = "last_run_id"
type = "string"
initial = ""

[[state]]
name = "user_message"
type = "string"
initial = ""

[[action]]
name = "Fire"
kind = "input"
from = ["Active"]
params = ["run_id", "user_message"]
effect = ["spawn('Run', 'Configure', last_run_id, params.run_id, copy(user_message))"]
"#;

/// A run refuses to start without a message, as a Session does.
const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Pending", "Running"]
initial = "Pending"

[[state]]
name = "user_message"
type = "string"
initial = ""

[[action]]
name = "Configure"
kind = "input"
from = ["Pending"]
to = "Running"
params = ["user_message"]

[[action.constraints]]
kind = "param_nonempty"
param = "user_message"
"#;

/// What one fire hands its run.
#[derive(Debug, Clone, Copy)]
enum Fire {
    /// A message: the run is created and starts.
    Message,
    /// An empty message: the run's `Configure` contract refuses it.
    EmptyMessage,
    /// A message, but the store fails this many reads of the new run.
    StoreFails(usize),
}

fn state_with_sim_store(seed: u64) -> (ServerState, SimEventStore) {
    let sim_store = SimEventStore::no_faults(seed);
    (state_with_store(sim_store.clone()), sim_store)
}

fn state_with_store(sim_store: SimEventStore) -> ServerState {
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[("Job", JOB_IOA), ("Run", RUN_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("dst-spawn-child"), registry);
    state.set_storage_stack(StorageStack::from_sim(sim_store.clone(), None));
    state
}

fn run_exists(store: &SimEventStore, run_id: &str) -> bool {
    !store
        .dump_journal(&format!("default:Run:{run_id}"))
        .is_empty()
}

/// The run ids a job's journal records as refused spawns.
fn refused_runs(store: &SimEventStore, job_id: &str) -> Vec<String> {
    store
        .dump_journal(&format!("default:Job:{job_id}"))
        .into_iter()
        .filter(|envelope| envelope.event_type == "SpawnRefused")
        .filter_map(|envelope| {
            envelope.payload["params"]["child_id"]
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

async fn wait_until_settled(store: &SimEventStore, job_id: &str, run_id: &str) {
    for _ in 0..SETTLE_POLLS {
        if run_exists(store, run_id) || refused_runs(store, job_id).iter().any(|id| id == run_id) {
            return;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn dst_a_parent_never_keeps_the_id_of_a_child_that_was_not_created() {
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let (state, store) = state_with_sim_store(seed);
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("dst-spawn-child");
        let mut rng = DeterministicRng::new(seed);

        let jobs = 3 + (rng.next_u64() % 3) as usize;
        let mut fired: Vec<(String, String, Fire)> = Vec::new();
        for job in 0..jobs {
            let job_id = format!("job-{seed}-{job}");
            state
                .get_or_create_tenant_entity(&tenant, "Job", &job_id, serde_json::json!({}))
                .await
                .expect("create Job");
            let fires = 1 + (rng.next_u64() % 3) as usize;
            for fire in 0..fires {
                let run_id = format!("run-{seed}-{job}-{fire}");
                let kind = match rng.next_u64() % 3 {
                    0 => Fire::Message,
                    1 => Fire::EmptyMessage,
                    _ => Fire::StoreFails(1 + (rng.next_u64() % 3) as usize),
                };
                let message = match kind {
                    Fire::EmptyMessage => "",
                    Fire::Message | Fire::StoreFails(_) => "summarise the day",
                };
                if let Fire::StoreFails(reads) = kind {
                    store.fail_next_reads(&format!("default:Run:{run_id}"), reads);
                }
                let response = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Job",
                        &job_id,
                        "Fire",
                        serde_json::json!({ "run_id": run_id, "user_message": message }),
                        &agent,
                    )
                    .await
                    .expect("Fire dispatches");
                assert!(
                    response.success,
                    "seed {seed}: Fire refused: {:?}",
                    response.error
                );
                // One fire settles before the next, as a schedule fires them.
                wait_until_settled(&store, &job_id, &run_id).await;
                fired.push((job_id.clone(), run_id, kind));
            }
        }

        for job in 0..jobs {
            let job_id = format!("job-{seed}-{job}");
            let stored = state
                .get_tenant_entity_state(&tenant, "Job", &job_id)
                .await
                .expect("read Job")
                .state
                .fields["last_run_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert!(
                stored.is_empty() || run_exists(&store, &stored),
                "seed {seed}: {job_id}.last_run_id names {stored}, which was never created"
            );
        }
        for (job_id, run_id, kind) in &fired {
            if run_exists(&store, run_id) {
                continue;
            }
            assert!(
                refused_runs(&store, job_id).contains(run_id),
                "seed {seed}: {run_id} ({kind:?}) was not created and {job_id}'s journal \
                 does not say so"
            );
        }
    }
}

#[tokio::test]
async fn dst_existing_child_survives_unreadable_journal() {
    for seed in 100..108 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let (state, store) = state_with_sim_store(seed);
        let tenant = TenantId::default();
        state
            .dispatch_tenant_action(
                &tenant,
                "Run",
                "existing",
                "Configure",
                serde_json::json!({"user_message": "hello"}),
                &AgentContext::for_service("test"),
            )
            .await
            .expect("create existing child");
        // A fresh server has no resident child. Its existence must come from
        // storage, whose first reads fail while a refused initializer settles.
        let fresh = state_with_store(store.clone());
        store.fail_next_reads("default:Run:existing", 5);
        let response = fresh
            .dispatch_tenant_action(
                &tenant,
                "Job",
                "job",
                "Fire",
                serde_json::json!({"run_id": "existing", "user_message": ""}),
                &AgentContext::for_service("test"),
            )
            .await
            .expect("fire");
        assert!(response.success);
        for _ in 0..SETTLE_POLLS {
            if !refused_runs(&store, "job").is_empty() {
                break;
            }
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
        assert_eq!(refused_runs(&store, "job"), vec!["existing"]);
        let fields = fresh
            .get_tenant_entity_state(&tenant, "Job", "job")
            .await
            .unwrap()
            .state
            .fields;
        assert_eq!(
            fields["last_run_id"], "existing",
            "seed {seed}: unreadability must not clear a durable child"
        );
    }
}

#[tokio::test]
async fn dst_failed_parent_refusal_append_eventually_converges() {
    for seed in 200..208 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let (state, store) = state_with_sim_store(seed);
        let tenant = TenantId::default();
        let response = state
            .dispatch_tenant_action(
                &tenant,
                "Job",
                "job",
                "Fire",
                serde_json::json!({"run_id": "absent", "user_message": ""}),
                &AgentContext::for_service("test"),
            )
            .await
            .expect("fire");
        assert!(response.success);
        // Exhaust the actor's refusal-append retries, then restore storage.
        store.inject_concurrency_violations("default:Job:job", 1000);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(refused_runs(&store, "job").is_empty());
        assert_eq!(
            state
                .get_tenant_entity_state(&tenant, "Job", "job")
                .await
                .unwrap()
                .state
                .fields["last_run_id"],
            "absent"
        );
        assert!(
            store
                .dump_journal("__temper_internal:SpawnRefusal:recovery")
                .iter()
                .any(|e| e.event_type == "Pending"),
            "repair must be durable before attempting the parent append"
        );
        store.inject_concurrency_violations("default:Job:job", 0);
        wait_until_settled(&store, "job", "absent").await;
        assert_eq!(refused_runs(&store, "job"), vec!["absent"]);
        assert_eq!(
            state
                .get_tenant_entity_state(&tenant, "Job", "job")
                .await
                .unwrap()
                .state
                .fields["last_run_id"],
            serde_json::Value::Null
        );
    }
}

#[test]
fn dst_restart_recovers_pending_refusal_and_retries_idempotently() {
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
    use temper_runtime::scheduler::{sim_now, sim_uuid};
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let store = runtime().block_on(async {
        let (_guard, _clock, _ids) = install_deterministic_context(300);
        let (state, store) = state_with_sim_store(300);
        let tenant = TenantId::default();
        state
            .get_or_create_tenant_entity(
                &tenant,
                "Job",
                "job",
                serde_json::json!({"last_run_id": "absent"}),
            )
            .await
            .unwrap();
        let journal = "__temper_internal:SpawnRefusal:recovery";
        store.append(journal, 0, &[PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "Pending".to_owned(),
        payload: serde_json::json!({"id": "restart-work", "work": {
            "tenant": "default", "parent_type": "Job", "parent_id": "job",
            "refusal": {"recovery_id": "repair-restart", "child_type": "Run", "child_id": "absent",
                "field": "last_run_id", "child_exists": false, "error": "initializer refused"}
        }}),
        metadata: EventMetadata {
            event_id: sim_uuid(), causation_id: sim_uuid(), correlation_id: sim_uuid(),
            timestamp: sim_now(), actor_id: journal.to_owned(),
        },
    }]).await.unwrap();
        store
    }); // Dropping the runtime cancels the first server's recovery worker.
    runtime().block_on(async {
        let (_guard, _clock, _ids) = install_deterministic_context(301);
        let tenant = TenantId::default();
        let journal = "__temper_internal:SpawnRefusal:recovery";
        // The parent commit can succeed while acknowledging the repair fails.
        store.inject_concurrency_violations(journal, 1000);
        let restarted = state_with_store(store.clone());
        wait_until_settled(&store, "job", "absent").await;
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(
            refused_runs(&store, "job"),
            vec!["absent"],
            "ack retries must not append duplicate parent events"
        );
        assert_eq!(
            restarted
                .get_tenant_entity_state(&tenant, "Job", "job")
                .await
                .unwrap()
                .state
                .fields["last_run_id"],
            serde_json::Value::Null
        );
    }); // Simulate another crash after parent commit but before acknowledgement.
    runtime().block_on(async {
        let (_guard, _clock, _ids) = install_deterministic_context(302);
        let journal = "__temper_internal:SpawnRefusal:recovery";
        store.inject_concurrency_violations(journal, 0);
        let restarted = state_with_store(store.clone());
        for _ in 0..SETTLE_POLLS {
            if store
                .dump_journal(journal)
                .iter()
                .any(|e| e.event_type == "Completed")
            {
                assert_eq!(
                    refused_runs(&store, "job"),
                    vec!["absent"],
                    "replay must retain repair idempotency"
                );
                assert_eq!(
                    restarted
                        .get_tenant_entity_state(&TenantId::default(), "Job", "job")
                        .await
                        .unwrap()
                        .state
                        .fields["last_run_id"],
                    serde_json::Value::Null
                );
                return;
            }
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
        panic!("recovered repair was never acknowledged");
    });
}
