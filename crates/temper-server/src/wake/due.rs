//! When each live timer of this process is due, so a wake can fire the ones
//! that fell due and the process can report the earliest one left.
//!
//! Every state timer and scheduled action registers here when its task starts
//! waiting ([`DueTimes::arm`]) and leaves when its task ends
//! ([`DueTimes::end`]). A timer waits for its delay or for a wake
//! ([`Armed::wait`]), whichever comes first, so a wake fires an overdue timer in the
//! task that would have fired it anyway: nothing fires twice, and a timer that
//! is already firing when a wake comes is waited for, not fired again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;
use tokio::sync::{Notify, oneshot};

use super::{WakeReport, WakeSink};
use crate::request_context::ScheduledFire;

/// The wait before an undelivered report is sent again; it doubles with each
/// try, up to [`LAST_RETRY_WAIT`].
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between tries: a control plane that is down for long is
/// tried about once an hour, not hammered.
const LAST_RETRY_WAIT: Duration = Duration::from_secs(3600);

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
    /// The earliest due time the control plane acknowledged last; None until
    /// it acknowledges one.
    acked: Option<Option<DateTime<Utc>>>,
    /// The report being sent, or sent again until delivered, by seq.
    in_flight: Option<(u64, Option<DateTime<Utc>>)>,
}

#[derive(Default)]
struct Inner {
    timers: BTreeMap<TimerRef, Entry>,
    by_due: BTreeSet<(DateTime<Utc>, TimerRef)>,
    next_id: u64,
    reported: Reported,
}

impl Inner {
    fn earliest(&self) -> Option<DateTime<Utc>> {
        self.by_due.first().map(|(at, _)| *at)
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

    /// Register `key`'s timer, due `at`, replacing the one it had: a newer
    /// arm of the same record's state timer supersedes the older one.
    pub(crate) fn arm(self: &Arc<Self>, key: TimerRef, at: DateTime<Utc>) -> Armed {
        let wake = Arc::new(Notify::new());
        let id = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            if let Some(replaced) = inner.remove(&key) {
                replaced.answer(false);
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
        self.report(false);
        Armed { key, id, wake }
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
        self.report(false);
    }

    /// `armed`'s task ended; `fired` says whether its action committed.
    pub(crate) fn end(self: &Arc<Self>, armed: &Armed, fired: bool) {
        let ended = {
            let mut inner = self.lock();
            if inner
                .timers
                .get(&armed.key)
                .is_none_or(|e| e.id != armed.id)
            {
                return;
            }
            inner.remove(&armed.key)
        };
        if let Some(entry) = ended {
            entry.answer(fired);
        }
        self.report(false);
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
        answers
    }

    /// The earliest due time of a live timer.
    pub(crate) fn earliest(&self) -> Option<DateTime<Utc>> {
        self.lock().earliest()
    }

    /// The earliest due time of a live timer of `tenant`.
    pub(crate) fn earliest_of(&self, tenant: &str) -> Option<DateTime<Utc>> {
        self.lock()
            .by_due
            .iter()
            .find(|(_, key)| key.tenant() == tenant)
            .map(|(at, _)| *at)
    }

    /// Send the earliest due time to the sink when the control plane has not
    /// acknowledged it and it is not on its way already, or always when
    /// `force`. A report that is not delivered is sent again, waiting longer
    /// after each failure, until it is delivered or a newer report replaces
    /// it: without it the control plane would not wake the process for that
    /// due time. The report carries the current span's trace: the move that
    /// changed the earliest due time.
    pub(crate) fn report(self: &Arc<Self>, force: bool) {
        let Some(sink) = self
            .sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let report = {
            let mut inner = self.lock();
            let earliest = inner.earliest();
            // What the control plane will believe once what is on its way
            // lands: the report in flight, or the last one acknowledged.
            let known = match inner.reported.in_flight {
                Some((_, at)) => at == earliest,
                None => inner.reported.acked == Some(earliest),
            };
            if known && !force {
                return;
            }
            inner.reported.seq += 1;
            let seq = inner.reported.seq;
            inner.reported.in_flight = Some((seq, earliest));
            WakeReport {
                next_due_at: earliest,
                reported_at: sim_now(),
                seq,
            }
        };
        let traceparent = crate::trace_context::traceparent(&tracing::Span::current());
        let due = self.clone();
        let send = async move {
            let mut wait = FIRST_RETRY_WAIT;
            loop {
                let result = sink.send(&report, traceparent.as_deref()).await;
                {
                    let mut inner = due.lock();
                    if inner
                        .reported
                        .in_flight
                        .is_none_or(|(seq, _)| seq != report.seq)
                    {
                        // A newer report replaced this one.
                        return;
                    }
                    if result.is_ok() {
                        inner.reported.in_flight = None;
                        inner.reported.acked = Some(report.next_due_at);
                        return;
                    }
                }
                if let Err(error) = &result {
                    tracing::warn!(seq = report.seq, %error, retry_in_ms = wait.as_millis() as u64,
                        "wake report not delivered; retrying");
                }
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                wait = (wait * 2).min(LAST_RETRY_WAIT);
            }
        };
        tokio::spawn(send); // determinism-ok: a report's delivery runs beside the commit
    }
}

impl Armed {
    /// Wait `delay`, or less if a wake comes first.
    pub(crate) async fn wait(&self, delay: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {} // determinism-ok: timer delay
            _ = self.wake.notified() => {}
        }
    }
}
