//! Durable rows for armed state timers, so a restart arms them again (ADR-0056).
//!
//! A timer writes its row when it is armed and removes it when it ends, so the
//! rows are the timers still waiting. At boot [`ServerState::rearm_state_timers`]
//! reads each tenant's rows, resolves declared arguments, and arms every
//! timer again from when its state's clock started: for the rest of its budget,
//! or at once if it fell due while the server was down. It fires once, as the
//! identity that armed it; ticks missed while the server was down are not
//! replayed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::state_timeouts::{ArmedTimer, declared_timer, remaining_budget};
use super::state_timer_follow::{BootCallers, BootTimers, log_boot_timers};
use crate::request_context::AgentContext;
use crate::request_context::armed_by::{decode_armed_by, encode_armed_by};
use crate::state::ServerState;
use crate::storage::{MetadataTimerRows, StateTimerStore};

/// How many timers armed at boot fire at once. Every timer that fell due while
/// the server was down is due at boot; firing them all together is a burst of
/// concurrent writes, and on 2026-10-16 1,610 at once on a loaded machine left
/// 861 events unwritten (`failed to persist event`) and 1,013 projections
/// failed, so those timers did not re-arm. The rest wait their turn.
pub(crate) const BOOT_FIRES_AT_ONCE: usize = 8;

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

    /// Write a newly armed timer's row. A failed write leaves the timer armed
    /// in this process; only a restart before it fires would lose it.
    pub(super) async fn persist_state_timer(&self, timer: &ArmedTimer) {
        let Some(store) = self.timer_rows_for_tenant(timer.tenant.as_str()).await else {
            return;
        };
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

    /// Remove an ended timer's row, unless a newer arm has replaced it.
    pub(super) async fn forget_state_timer(&self, timer: &ArmedTimer) {
        let Some(store) = self.timer_rows_for_tenant(timer.tenant.as_str()).await else {
            return;
        };
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
    /// Returns immediately; reads and retries run independently per tenant in
    /// the background, so unreadable rows cannot hold startup or other tenants.
    /// Reads each waiting timer and, when its declaration has arguments, its record,
    /// with at most [`BOOT_FIRES_AT_ONCE`] firing at once; a row whose state
    /// declares no timeout now is removed. Then, in the background, each
    /// record follows the state it is in now (as whoever armed
    /// its row when present), through the same gate. Missing rows are armed
    /// from the recorded state clock. The scheduled actions left
    /// waiting are armed again from their rows through the same gate
    /// (`rearm_scheduled_actions`). Returns that background walk, which ends
    /// with the boot's counts per tenant and type, and logs them as
    /// `state timers after boot`.
    pub(crate) async fn rearm_state_timers(&self) -> tokio::task::JoinHandle<BootTimers> {
        // Consume only the startup installs. Installs arriving while the
        // background recovery runs must remain queued for a later walk.
        self.registry
            .write()
            .expect("registry lock poisoned")
            .take_changed_types();
        let server = self.clone();
        // determinism-ok: durable recovery and its retries must not block HTTP startup.
        tokio::spawn(async move { server.recover_state_timers().await })
    }

    async fn recover_state_timers(&self) -> BootTimers {
        let gate = self.state_timer_boot_gate.clone();
        let mut counts = BootTimers::new();
        self.rearm_scheduled_actions(&gate, &mut counts).await;
        let mut tenants = FuturesUnordered::new();
        for (tenant, timeouts) in self.declared_state_timeouts() {
            let gate = gate.clone();
            tenants.push(async move {
                let mut types = timeouts
                    .keys()
                    .map(|(kind, _)| (tenant.clone(), kind.clone()))
                    .collect::<std::collections::BTreeSet<_>>();
                let (callers, mut counts) = self.recover_tenant_state_timers(&tenant).await;
                // A type can gain its first timeout while row reads retry.
                // Reconcile it before releasing the saved callers to queued
                // installs, including callers from rows just dropped above.
                {
                    let registry = self.registry.read().expect("registry lock poisoned");
                    types.extend(registry.entity_types(&tenant).into_iter().filter_map(
                        |entity_type| {
                            let spec = registry.get_spec(&tenant, entity_type)?;
                            (!spec.automaton.state_timeouts.is_empty())
                                .then(|| (tenant.clone(), entity_type.to_string()))
                        },
                    ));
                }
                // Only this tenant's saved identities are now known. Other
                // tenants may still be retrying; their records stay untouched.
                let followed = self
                    .follow_types(types.into_iter().collect(), &callers, Some(gate))
                    .await;
                self.timer_recovered_tenants
                    .write()
                    .expect("timer recovery lock poisoned")
                    .insert(tenant.clone());
                // Drain installs queued while this tenant was recovering,
                // independently of tenants still retrying their row reads.
                self.follow_spec_changes().await;
                for (key, followed) in followed {
                    let counted = counts.entry(key).or_default();
                    counted.waiting = followed.waiting;
                    counted.armed += followed.armed;
                    counted.overdue += followed.overdue;
                }
                counts
            });
        }
        while let Some(recovered) = tenants.next().await {
            for (key, recovered) in recovered {
                let counted = counts.entry(key).or_default();
                counted.rearmed += recovered.rearmed;
                counted.dropped += recovered.dropped;
                counted.waiting += recovered.waiting;
                counted.armed += recovered.armed;
                counted.overdue += recovered.overdue;
            }
        }
        {
            let _registry = self.registry.write().expect("registry lock poisoned");
            self.timer_recovery_complete
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.follow_spec_changes().await;
        log_boot_timers(&counts);
        counts
    }

    /// Restore a tenant's saved rows before allowing its rowless recovery.
    async fn recover_tenant_state_timers(&self, tenant: &TenantId) -> (BootCallers, BootTimers) {
        let started = Instant::now();
        let gate = self.state_timer_boot_gate.clone();
        let mut callers = BootCallers::new();
        let mut counts = BootTimers::new();
        let mut armed = 0usize;
        let mut overdue = 0usize;
        if let Some(store) = self.timer_rows_for_tenant(tenant.as_str()).await {
            // A failed read does not establish row absence. Restore saved
            // callers before any walk can arm missing rows as the service.
            let mut wait = Duration::from_secs(1);
            let rows = loop {
                match store.list_state_timers(tenant.as_str()).await {
                    Ok(rows) => break rows,
                    Err(error) => {
                        tracing::error!(tenant = %tenant, %error, retry_seconds = wait.as_secs(),
                            "state timer rows not readable; retrying before timer reconciliation");
                        tokio::time::sleep(wait).await; // determinism-ok: recovery retry on runtime clock
                        wait = (wait * 2).min(Duration::from_secs(60));
                    }
                }
            };
            for row in rows {
                // Row reads may have retried across an install. Saved rows
                // must not fire before its policies and modules are loaded.
                self.wait_for_timer_install(tenant).await;
                // Installs can change declarations while the row read retries.
                // Check the current spec before dropping a durable caller.
                let declared = {
                    let registry = self.registry.read().expect("registry lock poisoned");
                    registry
                        .get_spec(tenant, &row.entity_type)
                        .and_then(|spec| {
                            spec.automaton
                                .state_timeouts
                                .iter()
                                .find(|timeout| timeout.state == row.state)
                        })
                        .cloned()
                };
                let timer = match declared {
                    Some(timeout) => {
                        match armed_timer(tenant, &row, &timeout.on_timeout, timeout.after_seconds)
                        {
                            Ok(mut timer) => {
                                // Rows retain identity and the clock, but not the declaration
                                // that produced their arguments. An install may have changed
                                // it while the row read retried. Resolve the current args
                                // before reserving or firing this arm.
                                if timeout.args.is_empty() {
                                    timer.params = serde_json::json!({});
                                    Ok(timer)
                                } else {
                                    let mut wait = Duration::from_secs(1);
                                    let current = loop {
                                        if let Some(current) = self
                                            .current_record_state(
                                                tenant,
                                                &row.entity_type,
                                                &row.entity_id,
                                            )
                                            .await
                                        {
                                            break current;
                                        }
                                        tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                                        entity_id = %row.entity_id, retry_seconds = wait.as_secs(),
                                        "state timer record unreadable; retrying before rearming");
                                        tokio::time::sleep(wait).await; // determinism-ok: recovery retry on runtime clock
                                        wait = (wait * 2).min(Duration::from_secs(60));
                                    };
                                    timer.params = declared_timer(
                                        tenant,
                                        &row.entity_type,
                                        &row.entity_id,
                                        &timeout,
                                        &current,
                                        &timer.agent_ctx,
                                        timer.started_at,
                                    )
                                    .params;
                                    Ok(timer)
                                }
                            }
                            Err(reason) => Err(reason),
                        }
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
                let delay = remaining_budget(timer.started_at, timer.after_seconds, sim_now());
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
                counted.overdue += usize::from(delay.is_zero());
                self.spawn_state_timer(timer, delay, Some(gate.clone()), armed_seq);
                armed += 1;
            }
        }
        tracing::info!(tenant = %tenant, armed, overdue,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "re-armed state timers from the store");
        (callers, counts)
    }

    /// Every tenant's declared timeouts, including untimed tenants so later
    /// installs can proceed once their saved rows have been read.
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
            declared.push((tenant.clone(), timeouts));
        }
        declared
    }
}

/// The identity a row's timer fires as, if the row keeps a readable one.
fn row_armed_by(row: &StateTimerRecord) -> Option<AgentContext> {
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
