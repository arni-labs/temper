//! The store and clock of the state-timer simulation: what outlives one
//! server process.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicRng, SimClock};
use temper_server::storage::StateTimerStore;

/// Timer rows, kept the way the SQL stores keep them: one per record, a row
/// armed at a lower journal sequence never replacing a higher one, removed
/// only by the arming that wrote it. Each write lands after a seeded number
/// of scheduler turns, so writes of overlapping arms and cancellations land
/// in any order, as round trips to a real store do.
pub(crate) struct SimTimerRows {
    rows: Mutex<BTreeMap<(String, String, String), StateTimerRecord>>,
    rng: Mutex<DeterministicRng>,
    /// A write takes fewer scheduler turns than this to land.
    round_trip_turns: usize,
}

impl SimTimerRows {
    /// No rows, with write delays of up to 7 turns drawn from `seed`.
    pub(crate) fn new(seed: u64) -> Self {
        Self::with_round_trips(seed, 8)
    }

    /// No rows, with write delays of fewer than `turns` turns drawn from
    /// `seed`: longer writes overlap more of what runs beside them.
    pub(crate) fn with_round_trips(seed: u64, turns: usize) -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            rng: Mutex::new(DeterministicRng::new(seed ^ 0x7177_e125)),
            round_trip_turns: turns,
        }
    }

    /// The record's row now.
    pub(crate) fn row(&self, tenant: &str, id: &str) -> Option<StateTimerRecord> {
        let rows = self.rows.lock().unwrap();
        rows.get(&(tenant.to_string(), "Job".to_string(), id.to_string()))
            .cloned()
    }

    /// Wait the seeded number of scheduler turns a write takes to land.
    async fn round_trip(&self) {
        let turns = self.rng.lock().unwrap().next_bound(self.round_trip_turns);
        for _ in 0..turns {
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait::async_trait]
impl StateTimerStore for SimTimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        let key = (
            timer.tenant.clone(),
            timer.entity_type.clone(),
            timer.entity_id.clone(),
        );
        self.round_trip().await;
        let mut rows = self.rows.lock().unwrap();
        if rows
            .get(&key)
            .is_none_or(|row| timer.sequence_nr >= row.sequence_nr)
        {
            rows.insert(key, timer.clone());
        }
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        self.round_trip().await;
        let mut rows = self.rows.lock().unwrap();
        if rows.get(&key).is_some_and(|row| row.arm_id == arm_id) {
            rows.remove(&key);
        }
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }
}

/// Simulation time: the paused clock of the running process, carried across
/// restarts with the time the server was down.
pub(crate) struct ProcessClock {
    epoch: DateTime<Utc>,
    base: Mutex<(Option<tokio::time::Instant>, Duration)>,
}

impl ProcessClock {
    /// A clock at the simulation epoch, before any process ran.
    pub(crate) fn new() -> Self {
        Self {
            epoch: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            base: Mutex::new((None, Duration::ZERO)),
        }
    }

    /// Start counting from this process's clock. Call inside its runtime.
    pub(crate) fn begin(&self) {
        self.base.lock().unwrap().0 = Some(tokio::time::Instant::now());
    }

    /// Stop counting, and add `downtime` before the next process. Call inside
    /// the ending process's runtime.
    pub(crate) fn end(&self, downtime: Duration) {
        let mut base = self.base.lock().unwrap();
        let ran = base
            .0
            .take()
            .map(|origin| origin.elapsed())
            .unwrap_or_default();
        base.1 += ran + downtime;
    }
}

impl SimClock for ProcessClock {
    fn now(&self) -> DateTime<Utc> {
        let base = self.base.lock().unwrap();
        let ran = base.0.map(|origin| origin.elapsed()).unwrap_or_default();
        self.epoch + chrono::Duration::from_std(base.1 + ran).unwrap()
    }

    fn tick(&self) -> u64 {
        0
    }
}
