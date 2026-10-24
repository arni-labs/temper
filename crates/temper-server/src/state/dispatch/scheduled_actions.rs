//! Scheduled actions (`schedule` and `schedule_at`) as durable timers.
//!
//! The actor writes a scheduled action's row in the commit of the event that
//! armed it (`EntityActor::arm_scheduled_actions`), so the rows are the
//! scheduled actions still waiting. After the commit this process arms each
//! one ([`ServerState::arm_scheduled_actions`]); at boot
//! [`ServerState::rearm_scheduled_actions`] arms every row again, through the
//! boot gate, for the rest of its wait or at once if it fell due while the
//! server was down. Replay never arms one: the rows are the source of truth.
//!
//! A fire runs as whoever armed it, with an idempotency key from its arm id,
//! so a fire repeated after a restart (its event written, its row not yet
//! removed) is a duplicate and does not run again. Once it has fired, its row
//! is removed. A refused fire is logged here, and its dispatch records it in
//! the record's trajectory like any refused action.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::persistence::ScheduledActionRecord;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;
use tracing::Instrument;

use super::effects::PostDispatchContext;
use super::state_timer_follow::BootTimers;
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::request_context::armed_by::decode_armed_by;
use crate::state::ServerState;

/// A scheduled action armed in this process.
struct ScheduledTimer {
    tenant: TenantId,
    entity_type: String,
    entity_id: String,
    /// The row's id: the arming event's sequence and the action's index.
    arm_id: String,
    action: String,
    /// Who armed it; it fires as them.
    agent_ctx: AgentContext,
}

impl ServerState {
    /// Arm the scheduled actions a committed transition armed. Their rows
    /// were written with its event.
    pub(super) fn arm_scheduled_actions(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        for scheduled in &response.scheduled_actions {
            let timer = ScheduledTimer {
                tenant: ctx.tenant.clone(),
                entity_type: ctx.entity_type.to_string(),
                entity_id: ctx.entity_id.to_string(),
                arm_id: scheduled.arm_id.clone(),
                action: scheduled.action.clone(),
                agent_ctx: ctx.agent_ctx.for_background_task(),
            };
            self.spawn_scheduled_action(timer, Duration::from_secs(scheduled.delay_seconds), None);
        }
    }

    /// Arm again every scheduled action a previous process left waiting
    /// (`rearm_state_timers` calls it at boot), each through `gate`, and
    /// count them per tenant and type into `counts`. A row that cannot be
    /// read is removed: it can never fire.
    pub(super) async fn rearm_scheduled_actions(
        &self,
        gate: &Arc<Semaphore>,
        counts: &mut BootTimers,
    ) {
        let Some((store, _)) = self.event_journal() else {
            return;
        };
        let tenants = match self.registry.read() {
            Ok(registry) => registry
                .tenant_ids()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        for tenant in tenants {
            let rows = match store.list_scheduled_actions(tenant.as_str()).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(tenant = %tenant, %error,
                        "scheduled action rows not readable; scheduled actions not re-armed");
                    continue;
                }
            };
            let now = sim_now();
            for row in rows {
                let (timer, due_at) = match scheduled_timer(&tenant, &row) {
                    Ok(armed) => armed,
                    Err(reason) => {
                        tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                            entity_id = %row.entity_id, action = %row.action, %reason,
                            "scheduled action row dropped at boot");
                        self.forget_scheduled_action(
                            &tenant,
                            &row.entity_type,
                            &row.entity_id,
                            &row.arm_id,
                        )
                        .await;
                        continue;
                    }
                };
                counts
                    .entry((tenant.clone(), row.entity_type.clone()))
                    .or_default()
                    .scheduled += 1;
                let delay = (due_at - now).to_std().unwrap_or(Duration::ZERO);
                self.spawn_scheduled_action(timer, delay, Some(gate.clone()));
            }
        }
    }

    fn spawn_scheduled_action(
        &self,
        timer: ScheduledTimer,
        delay: Duration,
        gate: Option<Arc<Semaphore>>,
    ) {
        let agent = &timer.agent_ctx;
        let span = tracing::info_span!(
            "dispatch.scheduled_actions",
            workflow.root_entity_type = %agent.workflow_root_entity_type.as_deref().unwrap_or(&timer.entity_type),
            workflow.root_entity_id = %agent.workflow_root_entity_id.as_deref().unwrap_or(&timer.entity_id),
            workflow.run_id = %agent.workflow_run_id.clone()
                .unwrap_or_else(|| format!("{}:{}", timer.entity_type, timer.entity_id)),
            temper.action = %timer.action,
            entity_type = %timer.entity_type,
            entity_id = %timer.entity_id,
        );
        let server = self.clone();
        let fire = async move {
            tokio::time::sleep(delay).await; // determinism-ok: scheduled delay
            let _permit = match gate {
                Some(gate) => gate.acquire_owned().await.ok(),
                None => None,
            };
            server.fire_scheduled_action(&timer).await;
        };
        tokio::spawn(fire.instrument(span)); // determinism-ok: timer delivery is a background side-effect
    }

    /// Fire `timer` once, log a refusal, and remove its row.
    async fn fire_scheduled_action(&self, timer: &ScheduledTimer) {
        let mut agent = timer.agent_ctx.clone();
        agent.idempotency_key = Some(format!("schedule:{}", timer.arm_id));
        let outcome = self
            .dispatch_tenant_action(
                &timer.tenant,
                &timer.entity_type,
                &timer.entity_id,
                &timer.action,
                serde_json::json!({}),
                &agent,
            )
            .await;
        let refused = match outcome {
            Ok(response) if response.success => None,
            Ok(response) => Some(
                response
                    .error
                    .unwrap_or_else(|| "guard not met".to_string()),
            ),
            Err(error) => Some(error),
        };
        if let Some(error) = refused {
            tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, action = %timer.action, arm_id = %timer.arm_id,
                %error, "scheduled action refused when it fired");
        }
        self.forget_scheduled_action(
            &timer.tenant,
            &timer.entity_type,
            &timer.entity_id,
            &timer.arm_id,
        )
        .await;
    }

    /// Remove a scheduled action's row. A row left behind fires again at the
    /// next boot, as a duplicate that does not run.
    async fn forget_scheduled_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) {
        let Some((store, _)) = self.event_journal() else {
            return;
        };
        if let Err(error) = store
            .delete_scheduled_action(tenant.as_str(), entity_type, entity_id, arm_id)
            .await
        {
            tracing::warn!(tenant = %tenant, entity_type, entity_id, arm_id, %error,
                "fired scheduled action row not removed; the next boot finds it a duplicate");
        }
    }
}

/// The timer a row arms, and when it is due.
fn scheduled_timer(
    tenant: &TenantId,
    row: &ScheduledActionRecord,
) -> Result<(ScheduledTimer, DateTime<Utc>), String> {
    let due_at = DateTime::parse_from_rfc3339(&row.due_at)
        .map_err(|error| format!("due_at: {error}"))?
        .with_timezone(&Utc);
    let agent_ctx = decode_armed_by(&row.armed_by).map_err(|error| format!("armed_by: {error}"))?;
    let timer = ScheduledTimer {
        tenant: tenant.clone(),
        entity_type: row.entity_type.clone(),
        entity_id: row.entity_id.clone(),
        arm_id: row.arm_id.clone(),
        action: row.action.clone(),
        agent_ctx,
    };
    Ok((timer, due_at))
}
