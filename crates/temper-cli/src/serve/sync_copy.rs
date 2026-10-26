//! The database's Turso Cloud backup (Turso Sync).
//!
//! With `TURSO_SYNC_URL` set, the database is a local file and a change is
//! confirmed once it commits there. A background loop copies committed
//! changes to the remote: each push carries everything committed since the
//! last one, one push runs at a time, and a failed push is retried with
//! backoff, or on every tick once a confirmed move has waited over half the
//! push-lag budget. Answers never wait for it. The push can lag: past the
//! budget, the store refuses new moves until a push catches up (see
//! [`temper_store_turso::PUSH_LAG_BUDGET`]). The loop also checks a file the
//! start could not check, records how far the backup is behind, keeps the
//! local write-ahead log short, and restarts the server in the two cases
//! where only a restart helps.

use std::time::Duration;

use axum::Router;
use temper_store_turso::{CopyState, SyncCopy};
use tokio::time::Instant;

/// Default period of the background push.
const DEFAULT_UPKEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The longest wait between push attempts while they fail. Retrying every
/// second against a remote that is down for an hour fills the log and meets
/// the remote with a burst when it returns; waiting longer than this delays
/// the backup by as much once the remote is back.
const MAX_PUSH_BACKOFF: Duration = Duration::from_secs(30);

/// Checkpoint the local log once it passes this size. Turso Sync turns the
/// engine's automatic checkpoints off.
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;

/// Push attempts timing out in a row before the server exits. The sync
/// engine sends its requests one at a time, so one the remote never answers
/// holds every later push until its connection fails; the supervisor's
/// restart starts a fresh one. The start needs no remote, so the restart
/// serves again at once.
const TIMEOUTS_BEFORE_RESTART: u32 = 6;

/// Run the backup of `copy` beside the router. Answers do not wait for it,
/// so the router is returned as it is.
pub(super) fn with_backup(router: Router, copy: Option<SyncCopy>) -> Router {
    if let Some(copy) = copy {
        spawn_backup(copy);
    }
    router
}

/// When the next push is due: every tick while pushes succeed, and after a
/// failure a wait that doubles with each failure in a row, up to
/// [`MAX_PUSH_BACKOFF`].
struct Retry {
    interval: Duration,
    wait: Duration,
    next: Instant,
}

impl Retry {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            wait: interval,
            next: Instant::now(),
        }
    }

    fn due(&self) -> bool {
        Instant::now() >= self.next
    }

    fn succeeded(&mut self) {
        self.wait = self.interval;
        self.next = Instant::now();
    }

    /// Record a failure; returns how long until the next attempt.
    fn failed(&mut self) -> Duration {
        let wait = self.wait;
        self.next = Instant::now() + wait;
        self.wait = (wait * 2).min(MAX_PUSH_BACKOFF.max(self.interval));
        wait
    }
}

/// Push committed changes, check a file the start could not check, record
/// the backup's progress, checkpoint the local log, and restart when only a
/// restart helps.
fn spawn_backup(copy: SyncCopy) {
    let interval = std::env::var("TEMPER_TURSO_SYNC_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_UPKEEP_INTERVAL);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut retry = Retry::new(interval);
        loop {
            tick.tick().await;
            if let Err(error) = copy.recheck().await {
                tracing::debug!(%error, "turso.sync remote still unreachable");
            }
            // Behind with nothing of its own: the start brings it level, with
            // no change of the file's lost.
            if copy.state() == CopyState::Behind {
                tracing::error!(
                    "turso.sync the file is behind its remote copy; exiting so the restart brings it level"
                );
                std::process::exit(75);
            }
            // A move waiting long for the remote is pushed on every tick, so
            // the remote carries it as soon as it is back.
            let due = retry.due() || copy.push_is_pressing();
            if copy.state() == CopyState::Copying && due {
                match copy.push_committed().await {
                    Ok(()) => retry.succeeded(),
                    Err(error) => {
                        let wait = retry.failed();
                        tracing::warn!(%error, retry_in_ms = wait.as_millis() as u64, "turso.sync push failed; the backup retries");
                    }
                }
            }
            if copy.pushes_timed_out_in_a_row() >= TIMEOUTS_BEFORE_RESTART {
                tracing::error!(
                    timeouts = copy.pushes_timed_out_in_a_row(),
                    "turso.sync pushes keep timing out; exiting so the restart opens a fresh connection"
                );
                std::process::exit(75);
            }
            if let Err(error) = copy.record_backup().await {
                tracing::debug!(%error, "turso.sync could not read the backup's progress");
            }
            // Writes go on in every state, so the log is kept short in every state.
            if let Err(error) = copy.checkpoint_if_wal_over(MAX_WAL_BYTES).await {
                tracing::warn!(%error, "turso.sync checkpoint failed");
            }
        }
    });
}

#[cfg(test)]
#[path = "sync_copy_test.rs"]
mod tests;
