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
use serde::{Deserialize, Serialize};
use temper_authz::SecurityContext;
use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;

use super::state_timeouts::{ArmedTimer, remaining_budget};
use crate::request_context::AgentContext;
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

/// The identity part of an [`AgentContext`] that a timer row keeps. Runtime
/// budgets and the idempotency key are not kept: a timer fires as a new
/// background action.
#[derive(Serialize, Deserialize)]
struct ArmedBy {
    security_ctx: Option<SecurityContext>,
    agent_id: Option<String>,
    session_id: Option<String>,
    agent_type: Option<String>,
    intent: Option<String>,
    trace_id: Option<String>,
    parent_span_id: Option<String>,
    workflow_root_entity_type: Option<String>,
    workflow_root_entity_id: Option<String>,
    workflow_run_id: Option<String>,
    #[serde(default)]
    observation_metadata: BTreeMap<String, String>,
}

impl From<&AgentContext> for ArmedBy {
    fn from(agent: &AgentContext) -> Self {
        Self {
            security_ctx: agent.security_ctx.clone(),
            agent_id: agent.agent_id.clone(),
            session_id: agent.session_id.clone(),
            agent_type: agent.agent_type.clone(),
            intent: agent.intent.clone(),
            trace_id: agent.trace_id.clone(),
            parent_span_id: agent.parent_span_id.clone(),
            workflow_root_entity_type: agent.workflow_root_entity_type.clone(),
            workflow_root_entity_id: agent.workflow_root_entity_id.clone(),
            workflow_run_id: agent.workflow_run_id.clone(),
            observation_metadata: agent.observation_metadata.clone(),
        }
    }
}

impl From<ArmedBy> for AgentContext {
    fn from(armed_by: ArmedBy) -> Self {
        Self {
            security_ctx: armed_by.security_ctx,
            agent_id: armed_by.agent_id,
            session_id: armed_by.session_id,
            agent_type: armed_by.agent_type,
            intent: armed_by.intent,
            trace_id: armed_by.trace_id,
            parent_span_id: armed_by.parent_span_id,
            workflow_root_entity_type: armed_by.workflow_root_entity_type,
            workflow_root_entity_id: armed_by.workflow_root_entity_id,
            workflow_run_id: armed_by.workflow_run_id,
            observation_metadata: armed_by.observation_metadata,
            ..Self::default()
        }
    }
}

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

    /// Harness seam for the before run: main has no install follow-up.
    pub async fn follow_spec_changes(&self) {}

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

    /// Arm again every state timer that a previous process left waiting.
    ///
    /// Call once at boot, after specs, policies and modules are loaded and
    /// before the server takes requests. Reads one row per waiting timer and
    /// loads no entity: an entity is loaded only when its timer fires. At
    /// most [`BOOT_FIRES_AT_ONCE`] of these timers fire at once. Returns how
    /// many timers were armed.
    pub async fn rearm_state_timers(&self) -> usize {
        let started = Instant::now();
        let gate = Arc::new(Semaphore::new(BOOT_FIRES_AT_ONCE));
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
                        armed_timer(&tenant, &row, action).map(|timer| (*after_seconds, timer))
                    }
                    None => Err("its state no longer declares a timeout".to_string()),
                };
                let (after_seconds, timer) = match timer {
                    Ok(timer) => timer,
                    Err(reason) => {
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
                let delay = remaining_budget(timer.started_at, after_seconds, now);
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
        armed
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
        armed_by: serde_json::to_string(&ArmedBy::from(&timer.agent_ctx))?,
    })
}

fn armed_timer(
    tenant: &TenantId,
    row: &StateTimerRecord,
    action: &str,
) -> Result<ArmedTimer, String> {
    let started_at = DateTime::parse_from_rfc3339(&row.started_at)
        .map_err(|error| format!("started_at: {error}"))?
        .with_timezone(&Utc);
    let params = serde_json::from_str(&row.params).map_err(|error| format!("params: {error}"))?;
    let armed_by: ArmedBy =
        serde_json::from_str(&row.armed_by).map_err(|error| format!("armed_by: {error}"))?;
    Ok(ArmedTimer {
        tenant: tenant.clone(),
        entity_type: row.entity_type.clone(),
        entity_id: row.entity_id.clone(),
        state: row.state.clone(),
        action: action.to_string(),
        params,
        agent_ctx: armed_by.into(),
        arm_id: row.arm_id.clone(),
        sequence_nr: row.sequence_nr,
        started_at,
    })
}
