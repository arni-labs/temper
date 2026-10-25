//! Waking a suspended process: the earliest due time it reports, and the wake
//! that fires what fell due.
//!
//! A sandbox that holds this process may be suspended while nothing calls it,
//! and its timers wait on the process's monotonic clock, which may not
//! advance while it sleeps. So the process tells the control plane when it
//! next has work it could sleep through: the earliest due time of its timers
//! (state timers and scheduled actions) at or after the time it is surely
//! awake until, which is [`ServerState::set_wake_awake`] after its newest
//! inbound request. It sends a [`WakeReport`] to its [`WakeSink`] when that
//! time comes earlier than what the control plane holds. At that time the
//! control plane calls `POST /api/runtime/wake`, and
//! [`ServerState::wake_runtime`] fires every timer of the caller's tenant due
//! by the wall clock, in the task that would have fired it, waits for those
//! fires, and answers with a [`WakeOutcome`]. A wake with nothing due fires
//! nothing, so an early or repeated wake changes nothing.

mod due;
mod http;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
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
    /// changed the earliest due time.
    async fn send(&self, report: &WakeReport, traceparent: Option<&str>) -> Result<(), SendError>;
}

/// Why a report was not delivered, which decides whether it is sent again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// It did not get through: the network, a timeout, or a control plane
    /// that failed or asked for a later try. Sent again, waiting longer each
    /// time.
    Undelivered(String),
    /// The control plane refused it on purpose, with this status: the
    /// process's pass is bad or revoked, as when the app was deleted or its
    /// pass replaced. Not sent again, since a refused pass is refused on every
    /// retry; the next change or wake sends a new report, so a pass that comes
    /// back works again.
    Refused(u16),
}

/// A wake's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeOutcome {
    /// How many due timers fired and committed.
    pub fired: usize,
    /// The earliest due time left that the process could sleep through, by
    /// the same rule as a report.
    pub next_due_at: Option<DateTime<Utc>>,
    /// The process's clock when it answered, as a report's.
    pub reported_at: DateTime<Utc>,
    /// The next number of the process's reports: the answer is one of them,
    /// and the control plane orders it with them by `(reported_at, seq)`.
    pub seq: u64,
}

impl ServerState {
    /// Send reports to `sink` from now on. Call before
    /// [`Self::start_runtime`], which reports once the boot's timers are armed.
    pub fn set_wake_sink(&self, sink: std::sync::Arc<dyn WakeSink>) {
        self.due_times.set_sink(sink);
    }

    /// The sandbox stays awake for `awake` after each inbound request, so a
    /// timer due before then fires on its own and is not reported. Unset, the
    /// process may be suspended at any moment and reports its earliest due
    /// time from now. Set it below the time the sandbox really stays awake,
    /// or a timer due in between is slept through.
    pub fn set_wake_awake(&self, awake: std::time::Duration) {
        self.due_times.set_awake(awake);
    }

    /// An inbound request came: the sandbox stays awake from now. Every
    /// request through the kernel's routes notes this; one that came after a
    /// suspension makes the timers wait again by the wall clock.
    pub fn note_inbound(&self) {
        self.due_times.note_inbound();
    }

    /// The due time a report names now: the earliest one this process could
    /// sleep through.
    pub fn next_due_at(&self) -> Option<DateTime<Utc>> {
        self.due_times.reportable(None)
    }

    /// Fire every timer of `tenant` due by the wall clock now, wait for those
    /// fires, and answer the earliest due time left of `tenant`'s timers that
    /// the process could sleep through, by the same rule as a report. The
    /// answer is a report: it carries the next `seq` and `reported_at`, the
    /// control plane orders it with the reports, and the process holds it as
    /// delivered, so an early wake brings the fresh value. The process reports
    /// besides only when another tenant's timer is earlier. A timer that is firing
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
        let answer = self.due_times.answer(tenant.as_str());
        WakeOutcome {
            fired,
            next_due_at: answer.next_due_at,
            reported_at: answer.reported_at,
            seq: answer.seq,
        }
    }
}

/// Note every request that reaches the kernel's routes as inbound, whatever it
/// asks and however it is answered: any request keeps the sandbox awake.
pub(crate) async fn note_inbound(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    state.note_inbound();
    next.run(request).await
}
