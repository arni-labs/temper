//! A gauge of timer rows past their due time, per tenant: state timers,
//! scheduled actions, and the follow-on rows of moves whose follow-ons have
//! not settled (`follow_on`), due from their move's commit. It reads the rows, not the timers armed in this process,
//! so a row that no process armed, or whose fire never landed, still counts.
//! Without it the kernel saw an overdue timer only when something touched its
//! record: 20 release records sat in Queued for two days unseen.
//!
//! A repeating timer whose fire changes nothing writes nothing, so its row stays
//! at the time of its first look and this process holds when it looks next. Such
//! a row is due when that timer was armed for, the later of the two: a poll
//! waiting for its next look is not late, and one whose look does not land is,
//! from when that look was due. A fire stuck retrying keeps the time it was
//! armed for (`DueTimes::armed_for`), so it counts too.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::persistence::{OverdueTimerRow, StateTimerRecord};
use temper_runtime::scheduler::sim_now;

use crate::request_context::ScheduledFire;
use crate::state::ServerState;
use crate::storage::StateTimerStore;
use crate::wake::TimerRef;

impl ServerState {
    /// Sample each tenant's timer rows past due into
    /// `temper_timer_rows_overdue` and
    /// `temper_timer_rows_overdue_oldest_seconds`, by `kind`. One read of the
    /// rows' due-time index per tenant, each given `limit` to answer: a slow
    /// store skips that tenant's sample, logged, rather than holding the
    /// loop. The runtime metrics loop calls it every interval.
    pub async fn record_overdue_timer_rows(&self, limit: Duration) {
        let tenants: Vec<String> = {
            let Ok(registry) = self.registry.read() else {
                return;
            };
            registry
                .tenant_ids()
                .into_iter()
                .map(|tenant| tenant.to_string())
                .collect()
        };
        let now = sim_now();
        let at = now.to_rfc3339();
        for tenant in tenants {
            let Some(store) = self.timer_rows_for_tenant(&tenant).await else {
                continue;
            };
            match tokio::time::timeout(limit, store.overdue_timer_rows(&tenant, &at)).await {
                Ok(Ok(rows)) => {
                    let state_timer = |row: &OverdueTimerRow| TimerRef::State {
                        tenant: tenant.clone(),
                        entity_type: row.entity_type.clone(),
                        entity_id: row.entity_id.clone(),
                    };
                    let scheduled = |row: &OverdueTimerRow| {
                        TimerRef::Scheduled(ScheduledFire {
                            tenant: tenant.clone(),
                            entity_type: row.entity_type.clone(),
                            entity_id: row.entity_id.clone(),
                            arm_id: row.arm_id.clone(),
                        })
                    };
                    let (state_rows, state_late) =
                        self.past_due(&rows.state_timers, &state_timer, now);
                    crate::runtime_metrics::record_overdue_timer_rows(
                        &tenant,
                        "state_timeout",
                        state_rows,
                        state_late,
                    );
                    // A move's follow-on row is due when the move commits and
                    // waits only for its follow-ons to settle: it is told
                    // apart, so a schedule's lateness stays its own.
                    let (follow_ons, schedules): (Vec<_>, Vec<_>) =
                        rows.scheduled_actions.iter().cloned().partition(|row| {
                            crate::request_context::follow_ons::is_follow_row(&row.arm_id)
                        });
                    let (scheduled_rows, scheduled_late) =
                        self.past_due(&schedules, &scheduled, now);
                    crate::runtime_metrics::record_overdue_timer_rows(
                        &tenant,
                        "scheduled_action",
                        scheduled_rows,
                        scheduled_late,
                    );
                    let (follow_rows, follow_late) = self.past_due(&follow_ons, &scheduled, now);
                    crate::runtime_metrics::record_overdue_timer_rows(
                        &tenant,
                        "follow_on",
                        follow_rows,
                        follow_late,
                    );
                }
                Ok(Err(error)) => {
                    tracing::warn!(tenant, %error, "timer rows past due not readable");
                }
                Err(_) => {
                    tracing::warn!(
                        tenant,
                        limit_ms = limit.as_millis() as u64,
                        "timer rows past due not read in time"
                    );
                }
            }
        }
    }

    /// How many of `rows` are late at `now`, and how many seconds late the
    /// oldest is. A row whose due time cannot be read counts, with no lateness.
    fn past_due(
        &self,
        rows: &[OverdueTimerRow],
        timer: &impl Fn(&OverdueTimerRow) -> TimerRef,
        now: DateTime<Utc>,
    ) -> (u64, u64) {
        let mut count = 0;
        let mut oldest_seconds = 0;
        for row in rows {
            let Ok(row_due) = DateTime::parse_from_rfc3339(&row.due_at) else {
                count += 1;
                continue;
            };
            let row_due = row_due.with_timezone(&Utc);
            let due = self
                .due_times
                .armed_for(&timer(row))
                .map_or(row_due, |armed_for| armed_for.max(row_due));
            let Ok(late) = (now - due).to_std() else {
                continue;
            };
            count += 1;
            oldest_seconds = oldest_seconds.max(late.as_secs());
        }
        (count, oldest_seconds)
    }
}

/// A record's state timer past its due time, as a read says it
/// (`@temper.overdue`): the timer should have fired and has not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverdueTimer {
    /// When the timer was due, RFC 3339 in UTC.
    pub(crate) due_at: String,
    /// How long ago that was, in whole seconds.
    pub(crate) late_seconds: u64,
}

impl ServerState {
    /// The record's state timer, when its row is past due now. It reads the
    /// row, as the gauge of rows past due does
    /// ([`Self::record_overdue_timer_rows`]), so a row no process armed, or
    /// whose fire never landed, is told too; a repeating timer waiting for
    /// its next look is not late.
    ///
    /// `None` when the record has no timer row, the row is not yet due, or
    /// it could not be read (logged).
    ///
    /// Cost: one read of the record's timer row.
    pub(crate) async fn overdue_state_timer(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<OverdueTimer> {
        let row = self
            .stored_state_timer_row(tenant, entity_type, entity_id)
            .await
            .ok()
            .flatten()?;
        let timer = TimerRef::State {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        };
        self.late(&timer, row.due_at.as_deref()?, sim_now())
    }

    /// The state timers of `entity_type`'s records that are past due now, by
    /// record id, as [`Self::overdue_state_timer`] tells one: for a page of
    /// records, one read of the due-time index and not one per record.
    /// Empty when the rows could not be read (logged).
    pub(crate) async fn overdue_state_timers(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        entity_type: &str,
    ) -> std::collections::BTreeMap<String, OverdueTimer> {
        let now = sim_now();
        let Some(store) = self.timer_rows_for_tenant(tenant.as_str()).await else {
            return Default::default();
        };
        let rows = match store
            .overdue_timer_rows(tenant.as_str(), &now.to_rfc3339())
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type, %error,
                    "timer rows past due not readable for a read");
                return Default::default();
            }
        };
        rows.state_timers
            .iter()
            .filter(|row| row.entity_type == entity_type)
            .filter_map(|row| {
                let timer = TimerRef::State {
                    tenant: tenant.to_string(),
                    entity_type: row.entity_type.clone(),
                    entity_id: row.entity_id.clone(),
                };
                Some((row.entity_id.clone(), self.late(&timer, &row.due_at, now)?))
            })
            .collect()
    }

    /// How late `timer` is at `now`, when its row says it was due at
    /// `row_due` and that has passed: due when this process armed it for,
    /// when that is later, as [`Self::past_due`] counts it. `None` when it
    /// is not late or `row_due` cannot be read.
    fn late(&self, timer: &TimerRef, row_due: &str, now: DateTime<Utc>) -> Option<OverdueTimer> {
        let row_due = DateTime::parse_from_rfc3339(row_due)
            .ok()?
            .with_timezone(&Utc);
        let due = self
            .due_times
            .armed_for(timer)
            .map_or(row_due, |armed_for| armed_for.max(row_due));
        let late = (now - due).to_std().ok()?;
        Some(OverdueTimer {
            due_at: due.to_rfc3339(),
            late_seconds: late.as_secs(),
        })
    }
}

/// Write the due time on each row an older release wrote without one (or
/// under another budget), so the gauge of timers past due sees it. An update
/// only: a row whose timer fired and removed it meanwhile stays removed.
pub(super) async fn date_timer_rows(
    rows: Vec<(Arc<dyn StateTimerStore>, StateTimerRecord, String)>,
) {
    for (store, row, due_at) in rows {
        if let Err(error) = store
            .date_state_timer(
                &row.tenant,
                &row.entity_type,
                &row.entity_id,
                &row.arm_id,
                &due_at,
            )
            .await
        {
            tracing::warn!(tenant = %row.tenant, entity_type = %row.entity_type,
                entity_id = %row.entity_id, %error,
                "state timer row's due time not written; the next boot writes it");
        }
    }
}
