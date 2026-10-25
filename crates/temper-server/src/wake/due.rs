//! When each live timer of this process is due, so a wake can fire the ones
//! that fell due and the process can report the earliest one left.
//!
//! Every state timer and scheduled action registers here when its task starts
//! waiting ([`DueTimes::arm`]) and leaves when its task ends
//! ([`DueTimes::end`]). A timer waits until its wall-clock due time
//! ([`Armed::wait_until`]), checked again whenever a wake or a resume comes,
//! so a wake fires an overdue timer in the task that would have fired it
//! anyway: nothing fires twice, and a timer that is already firing when a wake
//! comes is waited for, not fired again.
//!
//! The process reports only the due times it could sleep through. Its sandbox
//! stays awake for a while after each inbound request ([`DueTimes::note_inbound`]),
//! and a timer due before then fires while it is awake, so the report names
//! the earliest due time at or after that horizon.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_authz::{Principal, PrincipalKind};
use temper_runtime::scheduler::sim_now;
use tokio::sync::{Notify, oneshot};

use super::{SendError, WakeReport, WakeSink, Waker};
use crate::request_context::ScheduledFire;

/// How far the wall clock may run ahead of the process's monotonic clock
/// between two inbound requests before the process counts itself resumed
/// from a suspension. It sits above scheduler jitter between two requests, so
/// a process that kept running never re-times its timers.
const RESUME_GAP: Duration = Duration::from_secs(1);

/// One live timer: a record's state timer (a record has at most one), or one
/// scheduled-action row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TimerRef {
    State {
        tenant: String,
        entity_type: String,
        entity_id: String,
    },
    Scheduled(ScheduledFire),
}

impl TimerRef {
    /// The least key, which sorts before every real one: a state timer with
    /// empty names, as the derived order puts `State` first.
    fn least() -> Self {
        Self::State {
            tenant: String::new(),
            entity_type: String::new(),
            entity_id: String::new(),
        }
    }

    fn tenant(&self) -> &str {
        match self {
            Self::State { tenant, .. } => tenant,
            Self::Scheduled(fire) => &fire.tenant,
        }
    }
}

/// A timer's registration, held by its task.
pub(crate) struct Armed {
    key: TimerRef,
    id: u64,
    wake: Arc<Notify>,
    retime: Arc<Notify>,
}

struct Entry {
    id: u64,
    at: DateTime<Utc>,
    wake: Arc<Notify>,
    /// Wakes waiting for this timer's fire: each hears whether it committed.
    waiters: Vec<oneshot::Sender<bool>>,
}

impl Entry {
    fn answer(self, fired: bool) {
        for waiter in self.waiters {
            let _ = waiter.send(fired);
        }
    }
}

/// What the control plane was last told, and what is on its way.
#[derive(Default)]
struct Reported {
    seq: u64,
    /// The due time the control plane acknowledged last; None until it
    /// acknowledges one.
    acked: Option<Option<DateTime<Utc>>>,
    /// The report being sent, or sent again until delivered, by seq.
    in_flight: Option<(u64, Option<DateTime<Utc>>)>,
}

#[derive(Default)]
struct Inner {
    timers: BTreeMap<TimerRef, Entry>,
    by_due: BTreeSet<(DateTime<Utc>, TimerRef)>,
    /// Replaced registrations still belong to their tasks: a state timer's
    /// fire can arm its successor before ending and answering its wakes.
    superseded: BTreeMap<(TimerRef, u64), Entry>,
    next_id: u64,
    reported: Reported,
    /// How long the sandbox stays awake after an inbound request; None: it
    /// may be suspended at any moment.
    awake: Option<chrono::Duration>,
    /// When the newest inbound request came.
    last_inbound: Option<DateTime<Utc>>,
    /// The wall clock and the monotonic clock at the last inbound request
    /// (or the start), to tell a resume from a process that kept running.
    clocks: Option<(DateTime<Utc>, tokio::time::Instant)>,
}

impl Inner {
    /// Until when the process is surely awake: a timer due before it fires on
    /// its own. Now, unless the sandbox is known to stay awake longer.
    fn horizon(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match (self.awake, self.last_inbound) {
            (Some(awake), Some(last)) => (last + awake).max(now),
            _ => now,
        }
    }

    /// The earliest due time at or after `horizon`, of `tenant`'s timers or of
    /// every timer: the one the process could sleep through.
    fn reportable(&self, horizon: DateTime<Utc>, tenant: Option<&str>) -> Option<DateTime<Utc>> {
        self.by_due
            .range((horizon, TimerRef::least())..)
            .find(|(_, key)| tenant.is_none_or(|tenant| key.tenant() == tenant))
            .map(|(at, _)| *at)
    }

    fn remove(&mut self, key: &TimerRef) -> Option<Entry> {
        let entry = self.timers.remove(key)?;
        self.by_due.remove(&(entry.at, key.clone()));
        Some(entry)
    }
}

/// The due time of every live timer in this process.
#[derive(Default)]
pub(crate) struct DueTimes {
    inner: Mutex<Inner>,
    sink: Mutex<Option<Arc<dyn WakeSink>>>,
    /// The agent principal id the control plane wakes the process as; None:
    /// no wake is the control plane's.
    control_plane: Mutex<Option<String>>,
    /// Tells every waiting timer to compute its wait again from the wall
    /// clock, after a resume.
    retime: Arc<Notify>,
}

impl DueTimes {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Where reports go. Without a sink, nothing is reported.
    pub(crate) fn set_sink(&self, sink: Arc<dyn WakeSink>) {
        *self
            .sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    /// The control plane wakes the process as the agent principal `id`.
    pub(crate) fn set_control_plane(&self, id: String) {
        *self
            .control_plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id);
    }

    /// Who woke the process: the control plane only when `principal` is the
    /// agent it wakes the process as. The control plane is never a person: a
    /// person's token names the app's owner, and the owner may wake the
    /// process by hand, so an id of a person could not tell the two apart.
    pub(crate) fn waker(&self, principal: &Principal) -> Waker {
        let control_plane = self
            .control_plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match control_plane.as_deref() {
            Some(id) if principal.kind == PrincipalKind::Agent && principal.id == id => {
                Waker::ControlPlane
            }
            _ => Waker::Other,
        }
    }

    /// The sandbox stays awake for `awake` after each inbound request.
    pub(crate) fn set_awake(&self, awake: Duration) {
        self.lock().awake = chrono::Duration::from_std(awake).ok();
    }

    /// The runtime started: note its clocks, and report.
    pub(crate) fn start(self: &Arc<Self>) {
        let wall = sim_now();
        let clock = tokio::time::Instant::now(); // determinism-ok: the process's own clock
        self.lock().clocks = Some((wall, clock));
        self.report();
    }

    /// An inbound request came, which keeps the sandbox awake. If the wall
    /// clock ran ahead of the monotonic clock since the last one, the process
    /// was suspended and its timers' waits ran on the stopped clock: every
    /// waiting timer computes its wait again, and an overdue one fires now.
    pub(crate) fn note_inbound(self: &Arc<Self>) {
        let wall = sim_now();
        let clock = tokio::time::Instant::now(); // determinism-ok: the process's own clock
        let resumed = {
            let mut inner = self.lock();
            let resumed = inner.clocks.is_some_and(|(last_wall, last_clock)| {
                let walked = (wall - last_wall).to_std().unwrap_or_default();
                walked.saturating_sub(clock.saturating_duration_since(last_clock)) > RESUME_GAP
            });
            inner.clocks = Some((wall, clock));
            inner.last_inbound = Some(inner.last_inbound.map_or(wall, |last| last.max(wall)));
            resumed
        };
        if resumed {
            tracing::info!(
                "process resumed from a suspension; timers wait again by the wall clock"
            );
            self.retime.notify_waiters();
        }
        self.report();
    }

    /// Register `key`'s timer, due `at`, replacing the one it had: a newer
    /// arm of the same record's state timer supersedes the older one.
    pub(crate) fn arm(self: &Arc<Self>, key: TimerRef, at: DateTime<Utc>) -> Armed {
        let wake = Arc::new(Notify::new());
        let id = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            if let Some(replaced) = inner.remove(&key) {
                inner
                    .superseded
                    .insert((key.clone(), replaced.id), replaced);
            }
            inner.by_due.insert((at, key.clone()));
            inner.timers.insert(
                key.clone(),
                Entry {
                    id,
                    at,
                    wake: wake.clone(),
                    waiters: Vec::new(),
                },
            );
            id
        };
        self.report();
        Armed {
            key,
            id,
            wake,
            retime: self.retime.clone(),
        }
    }

    /// `armed`'s timer did not commit and waits until `at` to try again:
    /// wakes waiting for it hear that it did not fire.
    pub(crate) fn retry_at(self: &Arc<Self>, armed: &Armed, at: DateTime<Utc>) {
        {
            let mut inner = self.lock();
            if inner
                .timers
                .get(&armed.key)
                .is_none_or(|e| e.id != armed.id)
            {
                return;
            }
            let Some(mut entry) = inner.remove(&armed.key) else {
                return;
            };
            for waiter in std::mem::take(&mut entry.waiters) {
                let _ = waiter.send(false);
            }
            entry.at = at;
            inner.by_due.insert((at, armed.key.clone()));
            inner.timers.insert(armed.key.clone(), entry);
        }
        self.report();
    }

    /// `armed`'s task ended; `fired` says whether its action committed.
    pub(crate) fn end(self: &Arc<Self>, armed: &Armed, fired: bool) {
        let ended = {
            let mut inner = self.lock();
            if let Some(entry) = inner.superseded.remove(&(armed.key.clone(), armed.id)) {
                Some(entry)
            } else if inner
                .timers
                .get(&armed.key)
                .is_some_and(|e| e.id == armed.id)
            {
                inner.remove(&armed.key)
            } else {
                return;
            }
        };
        if let Some(entry) = ended {
            entry.answer(fired);
        }
        self.report();
    }

    /// Wake every timer of `tenant` due by `now`, and hand back one answer
    /// per timer: whether its fire committed.
    pub(crate) fn wake_overdue(
        &self,
        tenant: &str,
        now: DateTime<Utc>,
    ) -> Vec<oneshot::Receiver<bool>> {
        let mut inner = self.lock();
        let overdue: Vec<TimerRef> = inner
            .by_due
            .iter()
            .take_while(|(at, _)| *at <= now)
            .filter(|(_, key)| key.tenant() == tenant)
            .map(|(_, key)| key.clone())
            .collect();
        let mut answers = Vec::with_capacity(overdue.len());
        for key in overdue {
            if let Some(entry) = inner.timers.get_mut(&key) {
                let (tx, rx) = oneshot::channel();
                entry.waiters.push(tx);
                entry.wake.notify_one();
                answers.push(rx);
            }
        }
        // Replacing a timer changes the next deadline, not the completion
        // of the old task. Concurrent wakes must still wait for that task.
        for ((key, _), entry) in &mut inner.superseded {
            if key.tenant() == tenant && entry.at <= now {
                let (tx, rx) = oneshot::channel();
                entry.waiters.push(tx);
                entry.wake.notify_one();
                answers.push(rx);
            }
        }
        answers
    }
}

impl Armed {
    /// Wait until the wall clock reaches `at`: for the time left now, on the
    /// process's monotonic clock. That clock stands still while the sandbox
    /// is suspended, so the time left is computed again when a wake or a
    /// resume comes: a timer that fell due meanwhile fires then.
    pub(crate) async fn wait_until(&self, at: DateTime<Utc>) {
        loop {
            let Ok(left) = (at - sim_now()).to_std() else {
                return;
            };
            if left.is_zero() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(left) => return, // determinism-ok: timer delay
                _ = self.wake.notified() => {}
                _ = self.retime.notified() => {}
            }
        }
    }
}

#[path = "due/report.rs"]
mod report;
#[cfg(test)]
#[path = "due/tests.rs"]
mod tests;
