//! A repeating timer keeps firing while every query projection fails.
//!
//! A committed transition cancels its record's old timer before its query
//! projection is written. When that write failed, the dispatch once answered
//! with the error and returned before arming the timer the committed state
//! declares, so the record had no timer until its next transition: a
//! repeating timer whose tick failed to project stopped for good. Since
//! #127 the timer is armed right after the commit and the dispatch keeps
//! trying the projection until it lands, so neither `Start` nor any `Tick`
//! is answered while the projection fails; the ticks must still come.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::projection_writes_fail::ProjectionWritesFail;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

/// A repeating timer: every `Tick` resets the clock for the next one.
const TICKER_IOA: &str = r#"
[automaton]
name = "Ticker"
states = ["Idle", "Running"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Running"]

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[action]]
name = "Start"
from = ["Idle"]
to = "Running"

[[action]]
name = "Tick"
from = ["Running"]
effect = ["ticks += 1"]

[[state_timeout]]
state = "Running"
after_seconds = 1
on_timeout = "Tick"
reset_on = ["Tick"]
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Timers" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Ticker"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Timers.Ticker"/></Action>
<Action Name="Tick" IsBound="true"><Parameter Name="bindingParameter" Type="Timers.Ticker"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Tickers" EntityType="Timers.Ticker"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A local store file, removed when the test ends.
struct StoreFile(std::path::PathBuf);

impl Drop for StoreFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn ticks(state: &ServerState) -> usize {
    state
        .get_tenant_entity_state(&TenantId::default(), "Ticker", "ticker-1")
        .await
        .expect("the ticker is readable")
        .state
        .counters
        .get("ticks")
        .copied()
        .unwrap_or(0)
}

#[tokio::test]
async fn a_transition_whose_projection_failed_still_arms_its_timer() {
    let file = StoreFile(std::env::temp_dir().join(format!(
        "temper-timer-projection-{}.db",
        uuid::Uuid::new_v4()
    )));
    let store = TursoEventStore::new(&format!("file:{}", file.0.display()), None)
        .await
        .expect("open local turso db");
    let (stack, failing) = ProjectionWritesFail::stack(store);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[("Ticker", TICKER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("timer-projection"), registry);
    state.set_storage_stack(stack);
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Ticker", "ticker-1", json!({}))
        .await
        .expect("the ticker is created");

    // From here every projection write fails: `Start` and each `Tick` commit,
    // and their dispatches wait for a projection that does not land.
    failing.store(true, Ordering::SeqCst);
    let starter = state.clone();
    let start = tokio::spawn(async move {
        starter
            .dispatch_tenant_action(
                &TenantId::default(),
                "Ticker",
                "ticker-1",
                "Start",
                json!({}),
                &AgentContext::default(),
            )
            .await
    });

    // Entering `Running` arms the first tick, and each tick the next.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = ticks(&state).await;
    while seen < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen = ticks(&state).await;
    }
    assert!(seen >= 2, "ticks after their projections failed: {seen}");
    assert!(
        !start.is_finished(),
        "Start waits for its projection (harness): the writes did fail"
    );
    start.abort();
}
