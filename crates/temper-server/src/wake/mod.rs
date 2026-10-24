//! Waking a suspended process: the earliest due time it reports, and the wake
//! that fires what fell due.
//!
//! A sandbox that holds this process may be suspended while nothing calls it,
//! and its timers wait on the process's monotonic clock, which may not
//! advance while it sleeps. So the process tells the control plane when it
//! next has work: each time a commit changes the earliest due time of its
//! timers (state timers and scheduled actions), it sends a [`WakeReport`] to
//! its [`WakeSink`]. At that time the control plane calls
//! `POST /api/runtime/wake`, and [`ServerState::wake_runtime`] fires every
//! timer of the caller's tenant due by the wall clock, in the task that would
//! have fired it, waits for those fires, and answers with a [`WakeOutcome`]. A wake with nothing
//! due fires nothing, so an early or repeated wake changes nothing.

mod due;
mod http;

use chrono::{DateTime, Utc};
use serde::Serialize;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

pub(crate) use due::{Armed, DueTimes, TimerRef};
pub use http::HttpWakeSink;

use crate::state::ServerState;

/// What the process tells the control plane: when it next has work. The
/// control plane orders reports by `reported_at`; `seq` counts this process's
/// reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeReport {
    /// The earliest due time of a live timer; None when nothing waits.
    pub next_due_at: Option<DateTime<Utc>>,
    pub reported_at: DateTime<Utc>,
    pub seq: u64,
}

/// Where reports go: the control plane, or a simulation's record of them.
#[async_trait::async_trait]
pub trait WakeSink: Send + Sync {
    /// Deliver one report, with the W3C `traceparent` of the move that
    /// changed the earliest due time. Err: not delivered.
    async fn send(&self, report: &WakeReport, traceparent: Option<&str>) -> Result<(), String>;
}

/// A wake's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeOutcome {
    /// How many due timers fired and committed.
    pub fired: usize,
    /// The earliest due time left.
    pub next_due_at: Option<DateTime<Utc>>,
}

impl ServerState {
    /// Send reports to `sink` from now on. Call before
    /// [`Self::start_runtime`], which reports once the boot's timers are armed.
    pub fn set_wake_sink(&self, sink: std::sync::Arc<dyn WakeSink>) {
        self.due_times.set_sink(sink);
    }

    /// The earliest due time of a live timer in this process.
    pub fn next_due_at(&self) -> Option<DateTime<Utc>> {
        self.due_times.earliest()
    }

    /// Fire every timer of `tenant` due by the wall clock now, wait for those
    /// fires, and answer the earliest due time of `tenant`'s timers left. The
    /// process's earliest due time is reported too. A timer that is firing
    /// already is waited for, not fired again; one whose fire does not commit
    /// keeps its row, tries again on its own and is not counted.
    ///
    /// Scoped to one tenant because a wake is authorized in one: a caller of
    /// one tenant neither fires another's timers nor learns when they are due.
    pub async fn wake_runtime(&self, tenant: &TenantId) -> WakeOutcome {
        let answers = self.due_times.wake_overdue(tenant.as_str(), sim_now());
        let mut fired = 0;
        for answer in answers {
            fired += usize::from(answer.await.unwrap_or(false));
        }
        let next_due_at = self.due_times.earliest_of(tenant.as_str());
        self.due_times.report(true);
        WakeOutcome { fired, next_due_at }
    }
}
