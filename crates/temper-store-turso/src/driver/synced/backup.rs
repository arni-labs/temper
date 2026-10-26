//! How far the remote copy is behind the file, for monitoring and for the
//! operator's `sync-copy status`.
//!
//! Recorded through OpenTelemetry:
//! - `temper_turso_sync_push_lag_changes`: committed changes the remote lacks.
//! - `temper_turso_sync_push_lag_seconds`: how long the remote has lacked one.
//! - `temper_turso_sync_push_failures_total`: push attempts that failed.
//! - `temper_turso_sync_push_in_flight`: 1 while a push runs.
//! - `temper_turso_sync_backup_stopped`: 1 while the backup is stopped
//!   ([`CopyState::Stopped`]); the cause is in the log and in `sync-copy status`.
//! - `temper_turso_sync_moves_refused`: 1 while new moves are refused because
//!   a confirmed one has waited for the remote past the push-lag budget.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Gauge};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tokio::time::Instant;

use super::remote::Mark;
use super::{CopyState, SyncCopy};

/// Where the backup of one database stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupStatus {
    /// What the backup can do now.
    pub state: CopyState,
    /// Changes committed to the file that the remote does not have yet.
    pub unpushed_changes: u64,
    /// How long the remote has lacked a committed change, as seen by the
    /// calls to [`SyncCopy::backup_status`]: zero once it has every one.
    pub behind_for: Duration,
    /// Whether a push is running now.
    pub pushing: bool,
    /// Push attempts that failed since this start.
    pub push_failures: u64,
    /// Whether new moves are refused until a push catches up.
    pub refusing_moves: bool,
}

/// Since when the remote has lacked a committed change.
#[derive(Debug, Default)]
pub(super) struct LagClock {
    behind_since: Option<Instant>,
}

impl LagClock {
    /// How long the remote has been behind, given how many committed changes
    /// it lacks now.
    pub(super) fn observe(&mut self, unpushed: u64) -> Duration {
        if unpushed == 0 {
            self.behind_since = None;
            return Duration::ZERO;
        }
        self.behind_since.get_or_insert_with(Instant::now).elapsed()
    }
}

struct Metrics {
    lag_changes: Gauge<u64>,
    lag_seconds: Gauge<f64>,
    failures: Counter<u64>,
    in_flight: Gauge<u64>,
    stopped: Gauge<u64>,
    moves_refused: Gauge<u64>,
}

fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = opentelemetry::global::meter("temper-store-turso");
        Metrics {
            lag_changes: meter
                .u64_gauge("temper_turso_sync_push_lag_changes")
                .with_description(
                    "Changes committed to a synced database that its remote copy lacks.",
                )
                .build(),
            lag_seconds: meter
                .f64_gauge("temper_turso_sync_push_lag_seconds")
                .with_unit("s")
                .with_description(
                    "How long a synced database's remote copy has lacked a committed change; 0 \
                     once it has every one.",
                )
                .build(),
            failures: meter
                .u64_counter("temper_turso_sync_push_failures_total")
                .with_description("Pushes to a synced database's remote copy that failed.")
                .build(),
            in_flight: meter
                .u64_gauge("temper_turso_sync_push_in_flight")
                .with_description("1 while a push to a synced database's remote copy runs.")
                .build(),
            stopped: meter
                .u64_gauge("temper_turso_sync_backup_stopped")
                .with_description(
                    "1 while a synced database's backup is stopped until an operator decides; \
                     the database keeps serving, and confirms moves within the push-lag budget.",
                )
                .build(),
            moves_refused: meter
                .u64_gauge("temper_turso_sync_moves_refused")
                .with_description(
                    "1 while a synced database refuses new moves because a confirmed one has \
                     waited for its remote copy past the push-lag budget.",
                )
                .build(),
        }
    })
}

/// Marks a push as running until dropped, a push given up on included.
pub(super) struct InFlight<'a>(&'a SyncCopy);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.inner.in_flight.store(false, Ordering::Release);
        metrics().in_flight.record(0, &[]);
    }
}

impl SyncCopy {
    /// Where the backup stands: how far the remote is behind the file, and
    /// what the backup is doing.
    pub async fn backup_status(&self) -> Result<BackupStatus, PersistenceError> {
        let local = self.local_mark().await?;
        let pushed_here = Mark {
            generation: local.generation,
            change: self.inner.pushed.load(Ordering::Acquire),
        };
        let pushed = self
            .last_pushed_hint()
            .map_err(storage_error)?
            .max(pushed_here);
        // A record from before the file's last pull covers none of its changes.
        let unpushed = if pushed.generation < local.generation {
            local.change
        } else {
            local.change - pushed.change
        };
        let unpushed = u64::try_from(unpushed).unwrap_or(0);
        let behind_for = self.inner.lag.lock().unwrap().observe(unpushed);
        Ok(BackupStatus {
            state: self.state(),
            unpushed_changes: unpushed,
            behind_for,
            pushing: self.inner.in_flight.load(Ordering::Acquire),
            push_failures: self.inner.failures.load(Ordering::Acquire),
            refusing_moves: self.refusing_moves(),
        })
    }

    /// [`Self::backup_status`], also recorded as the gauges in the module docs.
    pub async fn record_backup(&self) -> Result<BackupStatus, PersistenceError> {
        self.observe_budget();
        let status = self.backup_status().await?;
        let metrics = metrics();
        metrics.lag_changes.record(status.unpushed_changes, &[]);
        metrics
            .lag_seconds
            .record(status.behind_for.as_secs_f64(), &[]);
        let stopped = matches!(status.state, CopyState::Stopped(_));
        metrics.stopped.record(u64::from(stopped), &[]);
        metrics
            .moves_refused
            .record(u64::from(status.refusing_moves), &[]);
        Ok(status)
    }

    pub(super) fn push_running(&self) -> InFlight<'_> {
        self.inner.in_flight.store(true, Ordering::Release);
        metrics().in_flight.record(1, &[]);
        InFlight(self)
    }

    pub(super) fn push_failed(&self) {
        self.inner.failures.fetch_add(1, Ordering::AcqRel);
        metrics().failures.add(1, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn lag_runs_from_the_first_change_the_remote_lacks_until_it_has_all() {
        let mut lag = LagClock::default();
        assert_eq!(lag.observe(0), Duration::ZERO);
        assert_eq!(lag.observe(3), Duration::ZERO, "behind from now on");
        tokio::time::advance(Duration::from_secs(40)).await;
        assert_eq!(
            lag.observe(7),
            Duration::from_secs(40),
            "more changes, same start"
        );
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(lag.observe(0), Duration::ZERO, "caught up");
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            lag.observe(1),
            Duration::ZERO,
            "a new lag starts again from zero"
        );
    }
}
