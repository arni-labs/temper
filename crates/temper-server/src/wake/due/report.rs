//! Reporting: the due time the process could sleep through, sent to the
//! control plane when it is earlier than what the control plane holds, and a
//! wake's answer, which is a report too.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;

use super::{DueTimes, SendError, WakeReport, Waker};

/// The wait before an undelivered report is sent again; it doubles with each
/// try, up to [`LAST_RETRY_WAIT`].
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between tries: a control plane that is down for long is
/// tried about once an hour, not hammered.
const LAST_RETRY_WAIT: Duration = Duration::from_secs(3600);

impl DueTimes {
    /// The earliest due time at or after the horizon, of `tenant`'s timers
    /// or of every timer: what a report or a wake's answer names.
    pub(crate) fn reportable(&self, tenant: Option<&str>) -> Option<DateTime<Utc>> {
        let inner = self.lock();
        inner.reportable(inner.horizon(sim_now()), tenant)
    }

    /// Report when the due time the process could sleep through is earlier
    /// than what the control plane holds, or it holds nothing yet. A later
    /// one is not reported: the control plane wakes the process early, for
    /// nothing, and that wake brings the fresh value.
    pub(crate) fn report(self: &Arc<Self>) {
        self.send_when();
    }

    /// What a wake of `tenant` answers, as a report: the earliest due time at
    /// or after the horizon of `tenant`'s timers, with the next `seq` and the
    /// time now, so the control plane orders it with the reports. Answered to
    /// the control plane, it is what the control plane holds now, and a
    /// report still on its way is older and dropped. Answered to anyone else,
    /// the control plane never sees it, so what it holds and what is on its
    /// way stay as they are, and a report that is failing is still retried.
    /// When another tenant's timer is earlier, that is reported as any
    /// earlier time is.
    pub(crate) fn answer(self: &Arc<Self>, tenant: &str, waker: Waker) -> WakeReport {
        let answer = {
            let mut inner = self.lock();
            let reported_at = sim_now();
            let next_due_at = inner.reportable(inner.horizon(reported_at), Some(tenant));
            inner.reported.seq += 1;
            if waker == Waker::ControlPlane {
                inner.reported.in_flight = None;
                inner.reported.acked = Some(next_due_at);
            }
            WakeReport {
                next_due_at,
                reported_at,
                seq: inner.reported.seq,
            }
        };
        self.report();
        answer
    }

    /// Send the due time the process could sleep through to the sink when it
    /// is earlier than what the control plane will hold once what is on its
    /// way lands, or it holds nothing yet. A report that
    /// is not delivered is sent again, waiting longer after each failure,
    /// until it is delivered or a newer report replaces it: without it the
    /// control plane would not wake the process for that due time. A report
    /// the control plane refuses on purpose ([`SendError::Refused`]) is not
    /// sent again: its pass would be refused on every retry. The report
    /// carries the current span's trace: the move that changed the due time.
    fn send_when(self: &Arc<Self>) {
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
            let earliest = inner.reportable(inner.horizon(sim_now()), None);
            // What the control plane will hold once what is on its way lands:
            // the report in flight, or the last one acknowledged.
            let known = match inner.reported.in_flight {
                Some((_, at)) => Some(at),
                None => inner.reported.acked,
            };
            let send = match known {
                None => true,
                Some(known) => match (earliest, known) {
                    (Some(earliest), Some(known)) => earliest < known,
                    (Some(_), None) => true,
                    (None, _) => false,
                },
            };
            if !send {
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
                let error = {
                    let mut inner = due.lock();
                    if inner
                        .reported
                        .in_flight
                        .is_none_or(|(seq, _)| seq != report.seq)
                    {
                        // A newer report replaced this one.
                        return;
                    }
                    match result {
                        Ok(()) => {
                            inner.reported.in_flight = None;
                            inner.reported.acked = Some(report.next_due_at);
                            return;
                        }
                        Err(SendError::Refused(status)) => {
                            // Nothing acknowledged, nothing on its way: the
                            // next change or wake sends a new report.
                            inner.reported.in_flight = None;
                            drop(inner);
                            tracing::warn!(
                                seq = report.seq,
                                status,
                                "wake report refused by the control plane; not sent again"
                            );
                            return;
                        }
                        Err(SendError::Undelivered(error)) => error,
                    }
                };
                tracing::warn!(seq = report.seq, %error, retry_in_ms = wait.as_millis() as u64,
                    "wake report not delivered; retrying");
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                wait = (wait * 2).min(LAST_RETRY_WAIT);
            }
        };
        tokio::spawn(send); // determinism-ok: a report's delivery runs beside the commit
    }
}
