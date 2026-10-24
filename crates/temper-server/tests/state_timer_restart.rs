//! Armed state timers survive a server restart (ADR-0056).
//!
//! Each server runs in its own Tokio runtime; dropping the runtime kills its
//! in-memory timers as a process exit would. After the restart, a timer that
//! fell due while the server was down fires at once, one still waiting fires
//! when its budget ends (not a full budget after boot), each fires exactly
//! once and as the principal whose action armed it, and a repeating timer
//! does not replay the ticks it missed. Run against Turso always, and against
//! PostgreSQL when `TEMPER_TEST_POSTGRES_URL` names a disposable database.
use std::time::{Duration, Instant};

#[path = "state_timer_restart/recovery.rs"]
mod recovery;

use serde_json::{Value, json};
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;

/// `Fire` records who it ran as and counts its runs; it stays in `Armed`
/// and does not reset the timer, so the timer fires once.
const PROBE_IOA: &str = r#"
[automaton]
name = "Probe"
states = ["Idle", "Armed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Armed"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[state]]
name = "fires"
type = "counter"
initial = 0

[[action]]
name = "Arm"
from = ["Idle"]
to = "Armed"
params = []

[[action]]
name = "Note"
from = ["Armed"]
params = []

[[action]]
name = "Fire"
from = ["Armed"]
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
effect = ["fires += 1"]

[[state_timeout]]
state = "Armed"
after_seconds = 4
on_timeout = "Fire"
"#;

/// A repeating timer: every `Tick` resets the clock for the next one.
const TICKER_IOA: &str = r#"
[automaton]
name = "Ticker"
states = ["Idle", "Running"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Running"]
strict_action_params = true

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[action]]
name = "Start"
from = ["Idle"]
to = "Running"
params = []

[[action]]
name = "Tick"
from = ["Running"]
params = []
effect = ["ticks += 1"]

[[state_timeout]]
state = "Running"
after_seconds = 2
on_timeout = "Tick"
reset_on = ["Tick"]
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Probe" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Probe"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="fired_by" Type="Edm.String"/>
</EntityType>
<EntityType Name="Ticker"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Arm" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Note" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Ticker"/></Action>
<Action Name="Tick" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Ticker"/></Action>
<EntityContainer Name="Container">
<EntitySet Name="Probes" EntityType="Probe.Probe"/>
<EntitySet Name="Tickers" EntityType="Probe.Ticker"/>
</EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Test-only blanket permit: this file checks timers, not authorization.
const PERMIT_ALL: &str = "permit(principal, action, resource);";

#[derive(Clone)]
enum Backend {
    Turso(String),
    Postgres(String, PostgresSchema),
}

impl Backend {
    async fn open(&self) -> StorageStack {
        match self {
            Self::Turso(url) => {
                StorageStack::from_turso(TursoEventStore::new(url, None).await.unwrap())
            }
            Self::Postgres(url, schema) => {
                let pool = sqlx::PgPool::connect(url).await.unwrap();
                StorageStack::from_postgres(PostgresEventStore::with_schema(pool, schema.clone()))
            }
        }
    }
}

/// One server process: its own runtime, so dropping it kills its timers.
struct Server {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Server {
    /// Start a server on `backend`, the way `temper serve` boots: specs,
    /// policies, then the boot re-arm of timers left waiting.
    fn start(backend: &Backend, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                "default",
                temper_spec::csdl::parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[("Probe", PROBE_IOA), ("Ticker", TICKER_IOA)],
            );
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(backend.open().await);
            state
                .authz
                .reload_tenant_policies("default", PERMIT_ALL)
                .unwrap();
            state.start_runtime().await;
            state
        });
        Self { runtime, state }
    }

    /// Kill the server the way a process exit would: every task stops.
    fn kill(self) {
        let Self { runtime, state } = self;
        drop(state);
        runtime.shutdown_background();
    }

    fn act(&self, entity_type: &str, id: &str, action: &str, who: &AgentContext) {
        self.runtime.block_on(async {
            let tenant = TenantId::default();
            self.state
                .get_or_create_tenant_entity(&tenant, entity_type, id, json!({}))
                .await
                .unwrap();
            let response = self
                .state
                .dispatch_tenant_action(&tenant, entity_type, id, action, json!({}), who)
                .await
                .unwrap();
            assert!(response.success, "{action} on {id}: {:?}", response.error);
        });
    }

    /// Arm many records at once: each is created and sent
    /// `Arm` concurrently, so all are armed well inside one timer's budget.
    fn arm_all(&self, entity_type: &str, ids: &[String], who: &AgentContext) {
        self.runtime.block_on(async {
            let mut arming = tokio::task::JoinSet::new();
            for id in ids {
                let (state, id, who) = (self.state.clone(), id.clone(), who.clone());
                let entity_type = entity_type.to_string();
                arming.spawn(async move {
                    let tenant = TenantId::default();
                    state
                        .get_or_create_tenant_entity(&tenant, &entity_type, &id, json!({}))
                        .await
                        .unwrap();
                    let response = state
                        .dispatch_tenant_action(&tenant, &entity_type, &id, "Arm", json!({}), &who)
                        .await
                        .unwrap();
                    assert!(response.success, "Arm on {id}: {:?}", response.error);
                });
            }
            while let Some(armed) = arming.join_next().await {
                armed.unwrap();
            }
        });
    }

    /// `(counter, fired_by)` for one entity.
    fn read(&self, entity_type: &str, id: &str, counter: &str) -> (usize, Value) {
        self.runtime.block_on(async {
            let entity = self
                .state
                .get_tenant_entity_state(&TenantId::default(), entity_type, id)
                .await
                .unwrap();
            let count = entity.state.counters.get(counter).copied().unwrap_or(0);
            (count, entity.state.fields["fired_by"].clone())
        })
    }

    fn wait_for(
        &self,
        entity_type: &str,
        id: &str,
        counter: &str,
        at_least: usize,
        within: Duration,
    ) -> usize {
        let deadline = Instant::now() + within;
        loop {
            let (count, _) = self.read(entity_type, id, counter);
            if count >= at_least || Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(Duration::from_millis(50)); // determinism-ok: real-time restart test
        }
    }

    /// Remove every timer row, as for timers armed before rows were kept.
    fn forget_rows(&self) {
        self.runtime.block_on(async {
            let store = self
                .state
                .metadata_store_for_tenant("default")
                .await
                .unwrap();
            for row in store.list_state_timers("default").await.unwrap() {
                store
                    .delete_state_timer("default", &row.entity_type, &row.entity_id, &row.arm_id)
                    .await
                    .unwrap();
            }
        });
    }

    /// Every timer row the store holds.
    fn rows(&self) -> Vec<StateTimerRecord> {
        self.runtime.block_on(async {
            let store = self
                .state
                .metadata_store_for_tenant("default")
                .await
                .unwrap();
            store.list_state_timers("default").await.unwrap()
        })
    }

    /// Write a row back, as one whose delete never happened.
    fn restore_row(&self, row: &StateTimerRecord) {
        self.runtime.block_on(async {
            let store = self
                .state
                .metadata_store_for_tenant("default")
                .await
                .unwrap();
            store.upsert_state_timer(row).await.unwrap();
        });
    }

    /// Wait until the store holds a timer row for each of `ids`.
    fn wait_for_rows(&self, ids: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let rows = self.rows();
            if ids
                .iter()
                .all(|id| rows.iter().any(|row| row.entity_id == *id))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timer rows not written: {rows:?}"
            );
            std::thread::sleep(Duration::from_millis(20)); // determinism-ok: real-time restart test
        }
    }
}

fn person(id: &str) -> AgentContext {
    AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            id,
            PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    }
}

fn sleep_until(instant: Instant) {
    std::thread::sleep(instant.saturating_duration_since(Instant::now())); // determinism-ok: real-time restart test
}

/// The overdue and the due-soon timer fire once each, on time, as whoever armed them.
fn timers_fire_once_as_their_principal_after_restart(backend: &Backend) {
    let first = Server::start(backend, "timers-run-1");
    let t0 = Instant::now();
    first.act("Probe", "overdue", "Arm", &person("alice"));
    sleep_until(t0 + Duration::from_secs(2));
    first.act("Probe", "due-soon", "Arm", &person("bob"));
    first.wait_for_rows(&["overdue", "due-soon"]);
    first.kill();

    // Down long enough for `overdue` (due at t0+4s) to fall due; `due-soon`
    // is due at about t0+6s.
    sleep_until(t0 + Duration::from_millis(4_500));
    let second = Server::start(backend, "timers-run-2");
    let booted = Instant::now();

    assert_eq!(
        second.wait_for("Probe", "overdue", "fires", 1, Duration::from_secs(1)),
        1,
        "a timer that fell due while the server was down fires at once"
    );
    assert_eq!(
        second.read("Probe", "due-soon", "fires").0,
        0,
        "a waiting timer does not fire early"
    );
    sleep_until(t0 + Duration::from_millis(5_500));
    assert_eq!(
        second.read("Probe", "due-soon", "fires").0,
        0,
        "a waiting timer does not fire early"
    );
    // A full budget after boot would be t0+8.5s; the rest of its budget ends at t0+6s.
    assert_eq!(
        second.wait_for(
            "Probe",
            "due-soon",
            "fires",
            1,
            Duration::from_millis(1_800)
        ),
        1,
        "a waiting timer fires when its own budget ends"
    );

    // Past the point where a second firing of either would show.
    sleep_until(booted + Duration::from_secs(5));
    assert_eq!(
        second.read("Probe", "overdue", "fires"),
        (1, json!("alice"))
    );
    assert_eq!(second.read("Probe", "due-soon", "fires"), (1, json!("bob")));
    second.kill();

    // Both timers ended: a later restart has nothing to fire.
    let third = Server::start(backend, "timers-run-3");
    sleep_until(Instant::now() + Duration::from_secs(1));
    assert_eq!(third.read("Probe", "overdue", "fires").0, 1);
    assert_eq!(third.read("Probe", "due-soon", "fires").0, 1);
    third.kill();
}

/// A repeating timer that missed several ticks fires once, then keeps its pace.
fn repeating_timer_resumes_without_replaying_missed_ticks(backend: &Backend) {
    let first = Server::start(backend, "ticker-run-1");
    first.act("Ticker", "feed", "Start", &person("alice"));
    assert_eq!(
        first.wait_for("Ticker", "feed", "ticks", 1, Duration::from_secs(4)),
        1
    );
    first.wait_for_rows(&["feed"]);
    first.kill();

    // Down for more than three ticks.
    std::thread::sleep(Duration::from_secs(7)); // determinism-ok: real-time restart test
    let second = Server::start(backend, "ticker-run-2");
    let booted = Instant::now();
    assert_eq!(
        second.wait_for("Ticker", "feed", "ticks", 2, Duration::from_secs(1)),
        2,
        "the overdue tick fires at once"
    );
    sleep_until(booted + Duration::from_millis(1_500));
    assert_eq!(
        second.read("Ticker", "feed", "ticks").0,
        2,
        "missed ticks are not replayed"
    );
    assert_eq!(
        second.wait_for("Ticker", "feed", "ticks", 3, Duration::from_millis(1_500)),
        3,
        "the next tick comes a full period after the catch-up"
    );
    second.kill();
}

fn turso() -> (tempfile::TempDir, Backend) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file:{}", dir.path().join("events.db").display());
    (dir, Backend::Turso(url))
}

#[test]
fn turso_timers_fire_once_as_their_principal_after_restart() {
    let (_dir, backend) = turso();
    timers_fire_once_as_their_principal_after_restart(&backend);
}

#[test]
fn turso_repeating_timer_resumes_without_replaying_missed_ticks() {
    let (_dir, backend) = turso();
    repeating_timer_resumes_without_replaying_missed_ticks(&backend);
}

#[test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
fn postgres_timers_survive_restart() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let schema = PostgresSchema::new(format!(
        "state_timers_{}",
        temper_runtime::scheduler::sim_uuid().simple()
    ))
    .unwrap();
    runtime.block_on(async {
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        migration::run_migrations_in_schema(&pool, &schema)
            .await
            .unwrap();
    });
    let backend = Backend::Postgres(url.clone(), schema.clone());
    timers_fire_once_as_their_principal_after_restart(&backend);
    repeating_timer_resumes_without_replaying_missed_ticks(&backend);
    runtime.block_on(async {
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::query(&format!(
            "DROP SCHEMA {} CASCADE",
            schema.qualify_sql("{schema}").trim_end_matches('.')
        ))
        .execute(&pool)
        .await
        .unwrap();
    });
}
