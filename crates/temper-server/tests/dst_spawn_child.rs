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
    (state, sim_store)
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
