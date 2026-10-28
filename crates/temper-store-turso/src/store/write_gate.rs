use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use temper_runtime::persistence::PersistenceError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::TursoEventStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WritePriority {
    High,
    Low,
}

impl WritePriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

pub(super) fn configured_write_concurrency(is_remote: bool) -> usize {
    let default = if is_remote { 1 } else { 4 };
    std::env::var("TEMPER_TURSO_WRITE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .max(1)
}

/// How often a writer that gives way to others looks again. It delays only
/// the one looking, and is short against one commit on a slow drive (about
/// 170 ms on a Tensorlake drive, roll3, 26 Oct 2026).
const GIVE_WAY_RECHECK: Duration = Duration::from_millis(25);

/// The process-local queue for writes to one database: its permits, and the
/// writers waiting for one.
#[derive(Debug)]
pub(super) struct WriteGate {
    pub(super) permits: Arc<Semaphore>,
    /// How many permits there are. A turn of the change log's trim takes all.
    size: u32,
    /// High-priority writers waiting for a permit. Low-priority writers wait
    /// while there are any.
    high_waiting: AtomicUsize,
    /// Writers of either priority waiting for a permit. The trim waits while
    /// there are any.
    waiting: AtomicUsize,
}

impl WriteGate {
    pub(super) fn new(size: usize) -> Self {
        let size = u32::try_from(size.max(1)).unwrap_or(u32::MAX);
        Self {
            permits: Arc::new(Semaphore::new(size as usize)),
            size,
            high_waiting: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
        }
    }
}

impl TursoEventStore {
    /// Acquire the process-local Turso write lane.
    ///
    /// Remote Turso serializes writes at the database. Letting every actor open
    /// an immediate transaction concurrently turns ordinary write pressure into
    /// a lock/timeout storm. This gate makes the queue explicit in-process, so
    /// timeout budgets apply to the actual database operation instead of many
    /// requests racing for the same remote writer. Every journal append takes
    /// a high-priority permit, so new low-priority writes yield to it.
    pub(super) async fn acquire_write_permit(
        &self,
        operation: &'static str,
        priority: WritePriority,
    ) -> Result<OwnedSemaphorePermit, PersistenceError> {
        let started = std::time::Instant::now();
        let timeout = write_gate_wait_timeout();
        let waiting = Waiting::new(&self.write_gate, priority);

        if priority == WritePriority::Low {
            self.wait_for_high_priority_writers(operation, started, timeout)
                .await?;
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(write_gate_timeout_error(operation, priority, timeout));
        }

        let permit =
            tokio::time::timeout(remaining, self.write_gate.permits.clone().acquire_owned())
                .await
                .map_err(|_| write_gate_timeout_error(operation, priority, timeout))?
                .map_err(|_| write_gate_closed())?;
        drop(waiting);

        let waited = started.elapsed();
        if waited >= Duration::from_millis(100) {
            tracing::warn!(
                operation,
                priority = priority.as_str(),
                wait_ms = waited.as_millis() as u64,
                "turso.write_gate waited"
            );
        } else {
            tracing::debug!(
                operation,
                priority = priority.as_str(),
                wait_ms = waited.as_millis() as u64,
                "turso.write_gate acquired"
            );
        }

        Ok(permit)
    }

    /// The whole write gate, for one batch of the change log's trim
    /// ([`Self::trim_change_log`]).
    ///
    /// A batch must never keep a writer waiting, so it starts only when no
    /// writer waits at the gate, and it never queues there: a queued request
    /// for every permit would hold back writers that arrive behind it. Taking
    /// every permit also waits for writers that hold one to finish. A writer
    /// with a permit may still be waiting for the database's own write lock,
    /// where the trim would race it: the database's busy handler retries
    /// every 1 to 100 ms (turso_core 0.7.2 `busy.rs` 62-90), so a batch
    /// started right after another would win the lock ahead of it.
    async fn trim_turn(&self) -> Result<OwnedSemaphorePermit, PersistenceError> {
        let gate = &self.write_gate;
        loop {
            if gate.waiting.load(Ordering::Acquire) == 0 {
                match gate.permits.clone().try_acquire_many_owned(gate.size) {
                    Ok(turn) => return Ok(turn),
                    Err(tokio::sync::TryAcquireError::Closed) => return Err(write_gate_closed()),
                    Err(tokio::sync::TryAcquireError::NoPermits) => {}
                }
            }
            tokio::time::sleep(GIVE_WAY_RECHECK).await;
        }
    }

    async fn wait_for_high_priority_writers(
        &self,
        operation: &'static str,
        started: std::time::Instant,
        timeout: Duration,
    ) -> Result<(), PersistenceError> {
        while self.write_gate.high_waiting.load(Ordering::Acquire) > 0 {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(write_gate_timeout_error(
                    operation,
                    WritePriority::Low,
                    timeout,
                ));
            }
            tokio::time::sleep(remaining.min(GIVE_WAY_RECHECK)).await;
        }
        Ok(())
    }
}

/// The write gate as the change log's trim sees it.
pub(super) struct TrimTurns<'a>(pub(super) &'a TursoEventStore);

impl crate::driver::TrimWriters for TrimTurns<'_> {
    type Turn = OwnedSemaphorePermit;

    fn turn(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Turn, PersistenceError>> + Send {
        self.0.trim_turn()
    }

    fn waiting(&self) -> bool {
        self.0.write_gate.waiting.load(Ordering::Acquire) > 0
    }

    fn lock_wait(&self) -> Duration {
        self.0.append_attempt_timeout()
    }
}

/// Counts one writer as waiting at the gate until it has a permit, gives up,
/// or is dropped.
struct Waiting<'a> {
    gate: &'a WriteGate,
    priority: WritePriority,
}

impl<'a> Waiting<'a> {
    fn new(gate: &'a WriteGate, priority: WritePriority) -> Self {
        gate.waiting.fetch_add(1, Ordering::AcqRel);
        if priority == WritePriority::High {
            gate.high_waiting.fetch_add(1, Ordering::AcqRel);
        }
        Self { gate, priority }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.priority == WritePriority::High {
            self.gate.high_waiting.fetch_sub(1, Ordering::AcqRel);
        }
        self.gate.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

fn write_gate_wait_timeout() -> Duration {
    const DEFAULT_WRITE_GATE_WAIT_TIMEOUT_MS: u64 = 30_000;
    const MIN_WRITE_GATE_WAIT_TIMEOUT_MS: u64 = 100;

    let configured = std::env::var("TEMPER_TURSO_WRITE_GATE_WAIT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WRITE_GATE_WAIT_TIMEOUT_MS);

    Duration::from_millis(configured.max(MIN_WRITE_GATE_WAIT_TIMEOUT_MS))
}

fn write_gate_closed() -> PersistenceError {
    PersistenceError::Storage("Turso write gate closed".to_string())
}

fn write_gate_timeout_error(
    operation: &'static str,
    priority: WritePriority,
    timeout: Duration,
) -> PersistenceError {
    PersistenceError::Storage(format!(
        "{operation} ({}) waited more than {}ms for Turso write gate",
        priority.as_str(),
        timeout.as_millis()
    ))
}
