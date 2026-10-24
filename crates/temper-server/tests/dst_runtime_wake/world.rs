//! What the wake simulation's process runs on: its clock, its state timer
//! rows, and the control plane's end of its reports.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicRng, SimClock};
use temper_server::storage::StateTimerStore;
use temper_server::wake::{WakeReport, WakeSink};

/// Simulation time: the process's paused clock, plus every suspension, during
/// which the wall clock moves on and the process's clock does not.
pub(crate) struct Clock {
    epoch: DateTime<Utc>,
    origin: Mutex<Option<tokio::time::Instant>>,
    suspended: Mutex<Duration>,
}

impl Clock {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            origin: Mutex::new(None),
            suspended: Mutex::new(Duration::ZERO),
        }
    }

    /// Start counting from the process's clock. Call inside its runtime.
    pub(crate) fn begin(&self) {
        *self.origin.lock().unwrap() = Some(tokio::time::Instant::now());
    }

    /// Move the wall clock on by `by` while the process's timers stand still.
    pub(crate) fn suspend(&self, by: Duration) {
        *self.suspended.lock().unwrap() += by;
    }
}

impl SimClock for Clock {
    fn now(&self) -> DateTime<Utc> {
        let ran = self
            .origin
            .lock()
            .unwrap()
            .map(|origin| origin.elapsed())
            .unwrap_or_default();
        let suspended = *self.suspended.lock().unwrap();
        self.epoch + chrono::Duration::from_std(ran + suspended).unwrap()
    }

    fn tick(&self) -> u64 {
        0
    }
}

/// State timer rows, kept as the SQL stores keep them: one per record, a
/// lower sequence never replacing a higher one, removed only by its own arm.
#[derive(Default)]
pub(crate) struct TimerRows(Mutex<BTreeMap<(String, String, String), StateTimerRecord>>);

#[async_trait::async_trait]
impl StateTimerStore for TimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        let key = (
            timer.tenant.clone(),
            timer.entity_type.clone(),
            timer.entity_id.clone(),
        );
        let mut rows = self.0.lock().unwrap();
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
        let mut rows = self.0.lock().unwrap();
        if rows.get(&key).is_some_and(|row| row.arm_id == arm_id) {
            rows.remove(&key);
        }
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let rows = self.0.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }
}

/// The control plane's end of the reports: it keeps every report it was sent,
/// and refuses the ones the seed picks.
pub(crate) struct Reports {
    pub(crate) sent: Mutex<Vec<WakeReport>>,
    pub(crate) rng: Mutex<DeterministicRng>,
}

#[async_trait::async_trait]
impl WakeSink for Reports {
    async fn send(&self, report: &WakeReport, _traceparent: Option<&str>) -> Result<(), String> {
        self.sent.lock().unwrap().push(report.clone());
        if self.rng.lock().unwrap().next_bound(4) == 0 {
            return Err("simulated refusal".to_string());
        }
        Ok(())
    }
}
