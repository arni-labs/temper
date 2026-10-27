//! A record's timer is the one its committed row holds: it counts from the
//! event that entered its state, and a row that cannot be read is not
//! replaced by a guess.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_server::storage::StateTimerStore;

use super::*;

/// How long the move's commit takes: on 25 Oct a user's Temper took 7.5 to
/// 13 s for one.
const SLOW_COMMIT: Duration = Duration::from_secs(13);
/// The `Busy` timer's budget in `specs::TALLY`.
const BUDGET: Duration = Duration::from_secs(86400);

/// Run `body` per seed on a paused runtime whose clock is `sim_now()`, so an
/// event's timestamp and a timer's wait are on one clock.
fn on_process_clock<F, Fut>(seed: u64, body: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let clock = Arc::new(process_clock::ProcessClock::new());
    let _guard = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime builds");
    runtime.block_on(async {
        clock.begin();
        body().await;
    });
}

/// Move the paused clock to `at`, and let what falls due run.
async fn advance_to(at: tokio::time::Instant) {
    tokio::time::advance(at.saturating_duration_since(tokio::time::Instant::now())).await;
    settle().await;
}

fn timer_row(store: &SimEventStore, id: &str) -> Option<StateTimerRecord> {
    store
        .list_state_timers(TENANT)
        .into_iter()
        .find(|row| row.entity_id == id)
}

/// A `Hold` whose commit takes 13 s still times out a day after the `Hold`,
/// as its committed row says, and not a day after its commit returned.
#[test]
fn a_slow_commit_does_not_move_the_deadline() {
    for seed in 0..16 {
        on_process_clock(seed, || async move {
            let store = SimEventStore::no_faults(seed);
            let mut state = server(&store, seed);
            // The caller waits for the commit, as a site's move does.
            state.action_dispatch_timeout = SLOW_COMMIT * 2;
            let tenant = TenantId::new(TENANT);
            state
                .get_or_create_tenant_entity(&tenant, "Tally", "slow", json!({"Id": "slow"}))
                .await
                .unwrap();
            store.inject_append_delay(&journal_of("slow"), SLOW_COMMIT);
            let asked = tokio::time::Instant::now();
            let answer = state
                .dispatch_tenant_action(
                    &tenant,
                    "Tally",
                    "slow",
                    "Hold",
                    json!({"Mark": "hold"}),
                    &AgentContext::default(),
                )
                .await
                .unwrap();
            assert!(answer.success, "seed {seed}: {answer:?}");
            assert!(
                asked.elapsed() >= SLOW_COMMIT,
                "seed {seed}: the commit took {:?}",
                asked.elapsed()
            );
            settle().await;
            let row = timer_row(&store, "slow").expect("the Hold committed its timer row");

            advance_to(asked + BUDGET + Duration::from_secs(1)).await;
            assert_eq!(
                journal(&store, "slow").await.0.status,
                "Open",
                "seed {seed}: the timer did not fire by its committed deadline, {} plus a day",
                row.started_at
            );
        });
    }
}

/// The server's timer rows, whose reads fail while `unreadable` is set.
struct FlakyTimerRows {
    rows: rows::SimTimerRows,
    unreadable: AtomicBool,
}

#[async_trait::async_trait]
impl StateTimerStore for FlakyTimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.rows.upsert_state_timer(timer).await
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.rows
            .delete_state_timer(tenant, entity_type, entity_id, arm_id)
            .await
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err(PersistenceError::Storage(
                "injected: timer rows unreadable".into(),
            ));
        }
        self.rows.list_state_timers(tenant).await
    }
}

/// A timer whose row cannot be read when it falls due, after a move that
/// did not reset it, waits for the row and then fires once with the
/// committed arguments. It neither fires with arguments read from the
/// record now nor writes them over its row.
#[test]
fn an_unreadable_timer_row_is_not_replaced() {
    for seed in 0..16 {
        on_process_clock(seed, || async move {
            let store = SimEventStore::no_faults(seed);
            // The timeout carries the `notes` count the record had when it
            // entered `Busy`; nothing else moves it.
            let tally = TALLY
                .replace("effect = [\"schedule('Note', 3600)\"]\n", "")
                .replace(
                    "on_timeout = \"Expire\"",
                    "on_timeout = \"Expire\"\nargs = { Mark = \"notes\" }",
                )
                .replace(
                    "name = \"Expire\"\nkind = \"internal\"",
                    "name = \"Expire\"\nkind = \"internal\"\nparams = [{ name = \"Mark\", type = \"counter\" }]",
                );
            let mut state = server_with_tally(&store, seed, &tally);
            let rows = Arc::new(FlakyTimerRows {
                rows: rows::SimTimerRows(store.clone()),
                unreadable: AtomicBool::new(false),
            });
            state.state_timer_rows = Some(rows.clone());
            let tenant = TenantId::new(TENANT);
            state
                .get_or_create_tenant_entity(&tenant, "Tally", "flaky", json!({"Id": "flaky"}))
                .await
                .unwrap();
            let held = tokio::time::Instant::now();
            for (action, params) in [("Hold", json!({"Mark": "hold"})), ("Note", json!({}))] {
                // The `Note` moves the record without resetting its timer,
                // and from it on the timer row cannot be read.
                rows.unreadable.store(action == "Note", Ordering::SeqCst);
                let answer = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Tally",
                        "flaky",
                        action,
                        params,
                        &AgentContext::default(),
                    )
                    .await
                    .unwrap();
                assert!(answer.success, "seed {seed}: {action}: {answer:?}");
                settle().await;
            }
            let committed = timer_row(&store, "flaky").expect("the Hold committed its timer row");

            advance_to(held + BUDGET + Duration::from_secs(1)).await;
            assert_eq!(
                timer_row(&store, "flaky").as_ref(),
                Some(&committed),
                "seed {seed}: the unreadable row was replaced"
            );
            assert_eq!(
                journal(&store, "flaky").await.0.status,
                "Busy",
                "seed {seed}: the timer fired without its committed row"
            );

            rows.unreadable.store(false, Ordering::SeqCst);
            advance_to(tokio::time::Instant::now() + Duration::from_secs(120)).await;
            let events = store.read_events(&journal_of("flaky"), 0).await.unwrap();
            let expired = events
                .iter()
                .filter(|event| event.event_type == "Expire")
                .collect::<Vec<_>>();
            assert_eq!(expired.len(), 1, "seed {seed}: the timer fires once");
            let committed_params: serde_json::Value =
                serde_json::from_str(&committed.params).unwrap();
            assert_eq!(
                expired[0].payload["params"]["Mark"], committed_params["Mark"],
                "seed {seed}: the timer fired with arguments other than its committed ones"
            );
        });
    }
}
