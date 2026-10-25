//! Durable rows for armed state timers, so a restart arms them again (ADR-0056).
//!
//! A timer writes its row when it is armed and removes it when it ends, so the
//! rows are the timers still waiting. At boot [`ServerState::rearm_state_timers`]
//! reads each tenant's rows once, without loading any entity, and arms every
//! timer again from when its state's clock started: for the rest of its budget,
//! or at once if it fell due while the server was down. It fires once, as the
//! identity that armed it; ticks missed while the server was down are not
//! replayed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;

use super::state_timeouts::{ArmedTimer, remaining_budget};
use super::state_timer_follow::{BootCallers, BootTimers};
use crate::request_context::AgentContext;
use crate::request_context::armed_by::{decode_armed_by, encode_armed_by};
use crate::state::ServerState;
use crate::storage::{MetadataTimerRows, StateTimerStore};

/// How many timers armed at boot fire at once. Every timer that fell due while
/// the server was down is due at boot; firing them all together is a burst of
/// concurrent writes, and on 2026-10-16 1,610 at once on a loaded machine left
/// 861 events unwritten (`failed to persist event`) and 1,013 projections
/// failed, so those timers did not re-arm. The rest wait their turn.
const BOOT_FIRES_AT_ONCE: usize = 8;

/// A tenant's declared timeouts: `(entity type, state)` to
/// `(after_seconds, on_timeout)`.
type DeclaredTimeouts = BTreeMap<(String, String), (u64, String)>;

impl ServerState {
    /// Where `tenant`'s timer rows are kept: [`Self::state_timer_rows`] when
    /// set, else the tenant's metadata store. None: rows are not kept.
    async fn timer_rows_for_tenant(&self, tenant: &str) -> Option<Arc<dyn StateTimerStore>> {
        if let Some(rows) = self.state_timer_rows.clone() {
            return Some(rows);
        }
        let store = self.metadata_store_for_tenant(tenant).await?;
        Some(Arc::new(MetadataTimerRows(store)))
    }

    /// The record's timer row, when the event store commits timer rows with
    /// the events that change them ([`Self::entity_rows_follow_commits`]).
    /// None: no row, rows that do not follow commits, or a row that could
    /// not be read (logged; the caller then writes as if there were none).
    pub(super) async fn committed_state_timer_row(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<StateTimerRecord> {
        if !self.entity_rows_follow_commits() {
            return None;
        }
        let store = self.timer_rows_for_tenant(tenant.as_str()).await?;
        match store
            .state_timer_row(tenant.as_str(), entity_type, entity_id)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                    "state timer row not readable");
                None
            }
        }
    }

    /// Write a newly armed timer's row. A failed write leaves the timer armed
    /// in this process; only a restart before it fires would lose it. When
    /// rows follow commits the event that armed the timer usually wrote its
    /// row, and this writes only a row that is missing.
    pub(super) async fn persist_state_timer(&self, timer: &ArmedTimer) {
        let Some(store) = self.timer_rows_for_tenant(timer.tenant.as_str()).await else {
            return;
        };
        let committed = self
            .committed_state_timer_row(&timer.tenant, &timer.entity_type, &timer.entity_id)
            .await;
        if committed.is_some_and(|row| row.arm_id == timer.arm_id) {
            return;
        }
        let record = match timer_record(timer) {
            Ok(record) => record,
            Err(error) => {
                tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id, %error, "state timer row not encodable");
                return;
            }
        };
        if let Err(error) = store.upsert_state_timer(&record).await {
            tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, %error,
                "state timer row not written; a restart before it fires loses it");
        }
    }

    /// Remove an ended timer's row, unless a newer arm has replaced it. When
    /// rows follow commits the event that ended the timer usually removed or
    /// replaced its row, and this removes only a row the timer still holds.
    pub(super) async fn forget_state_timer(&self, timer: &ArmedTimer) {
        let Some(store) = self.timer_rows_for_tenant(timer.tenant.as_str()).await else {
            return;
        };
        if self.entity_rows_follow_commits() {
            let held = self
                .committed_state_timer_row(&timer.tenant, &timer.entity_type, &timer.entity_id)
                .await;
            if held.is_none_or(|row| row.arm_id != timer.arm_id) {
                return;
            }
        }
        if let Err(error) = store
            .delete_state_timer(
                timer.tenant.as_str(),
                &timer.entity_type,
                &timer.entity_id,
                &timer.arm_id,
            )
            .await
        {
            tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, %error,
                "ended state timer row not removed; the next boot finds it stale");
        }
    }

    /// Arm again every state timer that a previous process left waiting
    /// (`ServerState::start_runtime` calls it once at boot).
    ///
    /// Reads one row per waiting timer and loads no entity until its timer
    /// fires, at most [`BOOT_FIRES_AT_ONCE`] firing at once; a row whose state
    /// declares no timeout now is removed. Then, in the background, each
    /// record that had a row follows the state it is in now (as whoever armed
    /// its row), through the same gate. A record in a timed state with no row
    /// is not armed: it waits for its next action. The scheduled actions left
    /// waiting are armed again from their rows through the same gate
    /// (`rearm_scheduled_actions`). Returns that background walk, which ends
    /// with the boot's counts per tenant and type, and logs them as
    /// `state timers after boot`.
    pub(crate) async fn rearm_state_timers(&self) -> tokio::task::JoinHandle<BootTimers> {
        let started = Instant::now();
        let gate = Arc::new(Semaphore::new(BOOT_FIRES_AT_ONCE));
        let mut callers = BootCallers::new();
        let mut counts = BootTimers::new();
        let mut armed = 0usize;
        let mut overdue = 0usize;
        for (tenant, timeouts) in self.declared_state_timeouts() {
            let Some(store) = self.timer_rows_for_tenant(tenant.as_str()).await else {
                continue;
            };
            let rows = match store.list_state_timers(tenant.as_str()).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(tenant = %tenant, %error, "state timer rows not readable; timers not re-armed");
                    continue;
                }
            };
            let now = sim_now();
            for row in rows {
                let declared = timeouts.get(&(row.entity_type.clone(), row.state.clone()));
                let timer = match declared {
                    Some((after_seconds, action)) => {
                        armed_timer(&tenant, &row, action, *after_seconds)
                    }
                    None => {
                        if let Some(armed_by) = row_armed_by(&row) {
                            let key = (
                                tenant.clone(),
                                row.entity_type.clone(),
                                row.entity_id.clone(),
                            );
                            callers.insert(key, armed_by);
                        }
                        Err("its state no longer declares a timeout".to_string())
                    }
                };
                let counted = counts
                    .entry((tenant.clone(), row.entity_type.clone()))
                    .or_default();
                let mut timer = match timer {
                    Ok(timer) => timer,
                    Err(reason) => {
                        counted.dropped += 1;
                        tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                            entity_id = %row.entity_id, state = %row.state, %reason,
                            "state timer row dropped at boot");
                        let _ = store
                            .delete_state_timer(
                                tenant.as_str(),
                                &row.entity_type,
                                &row.entity_id,
                                &row.arm_id,
                            )
                            .await;
                        continue;
                    }
                };
                timer.boot_gate = Some(gate.clone());
                let delay = remaining_budget(timer.started_at, timer.after_seconds, now);
                let label = if delay.is_zero() {
                    "overdue"
                } else {
                    "budgeted"
                };
                overdue += usize::from(delay.is_zero());
                crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                    tenant.as_str(),
                    &timer.entity_type,
                    &timer.state,
                    label,
                );
                let Some(armed_seq) = self.reserve_state_timer(&timer) else {
                    continue;
                };
                counted.rearmed += 1;
                self.spawn_state_timer(timer, delay, Some(gate.clone()), armed_seq);
                armed += 1;
            }
        }
        tracing::info!(
            armed,
            overdue,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "re-armed state timers from the store"
        );
        self.rearm_scheduled_actions(&gate, &mut counts).await;
        {
            // Serialize with install walks so none can consume startup changes
            // before saved identities and gates have been restored. The boot
            // walk covers all timed types, with rows-only arming at startup.
            let mut registry = self.registry.write().expect("registry lock poisoned");
            registry.take_changed_types();
            self.timer_recovery_complete
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let server = self.clone();
        // determinism-ok: boot follow-up runs in the background, as boot timers do.
        tokio::spawn(async move { server.follow_timed_types(callers, gate, counts).await })
    }

    /// Every tenant's declared timeouts. Tenants without any are left out.
    fn declared_state_timeouts(&self) -> Vec<(TenantId, DeclaredTimeouts)> {
        let registry = self.registry.read().expect("registry lock poisoned");
        let mut declared = Vec::new();
        for tenant in registry.tenant_ids() {
            let mut timeouts = BTreeMap::new();
            for entity_type in registry.entity_types(tenant) {
                let Some(spec) = registry.get_spec(tenant, entity_type) else {
                    continue;
                };
                for st in &spec.automaton.state_timeouts {
                    timeouts.insert(
                        (entity_type.to_string(), st.state.clone()),
                        (st.after_seconds, st.on_timeout.clone()),
                    );
                }
            }
            if !timeouts.is_empty() {
                declared.push((tenant.clone(), timeouts));
            }
        }
        declared
    }
}

/// The identity a row's timer fires as, if the row keeps a readable one.
pub(super) fn row_armed_by(row: &StateTimerRecord) -> Option<AgentContext> {
    decode_armed_by(&row.armed_by).ok()
}

fn timer_record(timer: &ArmedTimer) -> Result<StateTimerRecord, serde_json::Error> {
    Ok(StateTimerRecord {
        tenant: timer.tenant.to_string(),
        entity_type: timer.entity_type.clone(),
        entity_id: timer.entity_id.clone(),
        state: timer.state.clone(),
        arm_id: timer.arm_id.clone(),
        sequence_nr: timer.sequence_nr,
        started_at: timer.started_at.to_rfc3339(),
        params: serde_json::to_string(&timer.params)?,
        armed_by: encode_armed_by(&timer.agent_ctx)?,
    })
}

fn armed_timer(
    tenant: &TenantId,
    row: &StateTimerRecord,
    action: &str,
    after_seconds: u64,
) -> Result<ArmedTimer, String> {
    let started_at = DateTime::parse_from_rfc3339(&row.started_at)
        .map_err(|error| format!("started_at: {error}"))?
        .with_timezone(&Utc);
    let params = serde_json::from_str(&row.params).map_err(|error| format!("params: {error}"))?;
    let armed_by = decode_armed_by(&row.armed_by).map_err(|error| format!("armed_by: {error}"))?;
    Ok(ArmedTimer {
        boot_gate: None,
        tenant: tenant.clone(),
        entity_type: row.entity_type.clone(),
        entity_id: row.entity_id.clone(),
        state: row.state.clone(),
        action: action.to_string(),
        params,
        after_seconds,
        agent_ctx: armed_by,
        arm_id: row.arm_id.clone(),
        sequence_nr: row.sequence_nr,
        started_at,
    })
}
