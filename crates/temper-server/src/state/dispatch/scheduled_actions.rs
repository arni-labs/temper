//! Scheduled actions (`schedule` and `schedule_at`) as durable timers.
//!
//! The actor writes a scheduled action's row in the commit of the event that
//! armed it (`EntityActor::arm_scheduled_actions`), so the rows are the
//! scheduled actions still waiting. After the commit, and after the
//! transition's integrations, this process arms each one
//! ([`ServerState::arm_scheduled_actions`]) on every path where the event was
//! written, and at once when the transition's projection write fails; at boot
//! [`ServerState::rearm_scheduled_actions`] arms every row again, through the
//! boot gate, for the rest of its wait or at once if it fell due while the
//! server was down. Replay never arms one: the rows are the source of truth.
//! A process arms each row at most once, however many replies hand it back.
//!
//! A fire runs as whoever armed it. The event it writes removes its row in the
//! same commit, so a row is removed only once its action has committed, and a
//! restart cannot fire it twice. A fire whose write failed or may have landed
//! keeps its row and is retried on it, with the same idempotency key from the
//! arm id: a write that did land is a duplicate and does not run again. A
//! refused fire is final: it is logged here, its dispatch records it in the
//! record's trajectory like any refused action, and then its row is removed.

use std::collections::BTreeMap;
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
use crate::request_context::armed_by::decode_armed_by;
use crate::request_context::{AgentContext, ScheduledFire};
use crate::state::ServerState;

/// The wait before a fire that did not commit is tried again; it doubles
/// with each try, up to [`LAST_RETRY_WAIT`].
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between tries: a store that is down for long is tried
/// about once an hour, not hammered, and its row keeps the fire across
/// restarts.
const LAST_RETRY_WAIT: Duration = Duration::from_secs(3600);

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

impl ScheduledTimer {
    /// The row this timer fires.
    fn row(&self) -> ScheduledFire {
        ScheduledFire {
            tenant: self.tenant.as_str().to_string(),
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            arm_id: self.arm_id.clone(),
        }
    }
}

impl ServerState {
    /// The scheduled actions armed in this process: each row's live timers.
    pub fn armed_scheduled_actions(&self) -> BTreeMap<ScheduledFire, usize> {
        self.scheduled_armed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

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
    /// read is removed: it can never fire. A tenant whose rows cannot be
    /// listed is tried again in the background until they can.
    pub(super) async fn rearm_scheduled_actions(
        &self,
        gate: &Arc<Semaphore>,
        counts: &mut BootTimers,
    ) {
        let tenants = match self.registry.read() {
            Ok(registry) => registry
                .tenant_ids()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        for tenant in tenants {
            if let Err(error) = self.rearm_tenant_rows(&tenant, gate, counts).await {
                tracing::error!(tenant = %tenant, %error,
                    retry_in_ms = FIRST_RETRY_WAIT.as_millis() as u64,
                    "scheduled action rows not readable at boot; retrying");
                self.retry_tenant_rows(tenant, gate.clone());
            }
        }
    }

    /// List `tenant`'s rows again until they can be read, and arm them.
    fn retry_tenant_rows(&self, tenant: TenantId, gate: Arc<Semaphore>) {
        let server = self.clone();
        let retry = async move {
            let mut wait = FIRST_RETRY_WAIT;
            loop {
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                let mut counts = BootTimers::default();
                match server.rearm_tenant_rows(&tenant, &gate, &mut counts).await {
                    Ok(()) => return,
                    Err(error) => {
                        wait = (wait * 2).min(LAST_RETRY_WAIT);
                        tracing::error!(tenant = %tenant, %error,
                            retry_in_ms = wait.as_millis() as u64,
                            "scheduled action rows still not readable; retrying");
                    }
                }
            }
        };
        tokio::spawn(retry); // determinism-ok: boot recovery retry is a background side-effect
    }

    /// Arm every row of `tenant` this process has not armed, through `gate`.
    async fn rearm_tenant_rows(
        &self,
        tenant: &TenantId,
        gate: &Arc<Semaphore>,
        counts: &mut BootTimers,
    ) -> Result<(), String> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(());
        };
        let rows = store
            .list_scheduled_actions(tenant.as_str())
            .await
            .map_err(|error| error.to_string())?;
        let now = sim_now();
        for row in rows {
            let (timer, due_at) = match scheduled_timer(tenant, &row) {
                Ok(armed) => armed,
                Err(reason) => {
                    tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                        entity_id = %row.entity_id, action = %row.action, %reason,
                        "scheduled action row dropped at boot");
                    self.forget_scheduled_action(
                        tenant,
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
        Ok(())
    }

    /// Arm the rows `timer`'s fire committed, after a retried fire. A fire
    /// whose answer was lost commits its successors' rows, but the duplicate
    /// that answers its retry hands back no schedules. Only the fire's own
    /// rows are armed: any other row of the record belongs to a dispatch that
    /// arms it itself, after its integrations. Err: the journal or the rows
    /// could not be read, and nothing was armed.
    async fn arm_fired_rows(&self, timer: &ScheduledTimer, key: &str) -> Result<(), String> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(());
        };
        // The fire's event comes after the event that armed it.
        let armed_seq = timer
            .arm_id
            .split_once('.')
            .and_then(|(seq, _)| seq.parse::<u64>().ok())
            .unwrap_or(0);
        let persistence_id = format!("{}:{}:{}", timer.tenant, timer.entity_type, timer.entity_id);
        let events = store
            .read_events(&persistence_id, armed_seq.saturating_sub(1))
            .await
            .map_err(|error| error.to_string())?;
        let Some(fire_seq) = events
            .iter()
            .find(|event| event.payload["idempotency_key"].as_str() == Some(key))
            .map(|event| event.sequence_nr)
        else {
            return Ok(());
        };
        let rows = store
            .list_scheduled_actions(timer.tenant.as_str())
            .await
            .map_err(|error| error.to_string())?;
        let prefix = format!("{fire_seq}.");
        let now = sim_now();
        let fired = rows.iter().filter(|row| {
            row.entity_type == timer.entity_type
                && row.entity_id == timer.entity_id
                && row.arm_id.starts_with(&prefix)
        });
        for row in fired {
            if let Ok((waiting, due_at)) = scheduled_timer(&timer.tenant, row) {
                let delay = (due_at - now).to_std().unwrap_or(Duration::ZERO);
                self.spawn_scheduled_action(waiting, delay, None);
            }
        }
        Ok(())
    }

    /// [`Self::arm_fired_rows`] until the rows can be read, waiting longer
    /// after each failure: until then the fire's successors have no timer.
    async fn arm_fired_rows_retrying(&self, timer: &ScheduledTimer, key: &str) {
        let mut wait = FIRST_RETRY_WAIT;
        while let Err(error) = self.arm_fired_rows(timer, key).await {
            tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, arm_id = %timer.arm_id, %error,
                retry_in_ms = wait.as_millis() as u64,
                "rows of a retried scheduled fire not readable; retrying");
            tokio::time::sleep(wait).await; // determinism-ok: retry backoff
            wait = (wait * 2).min(LAST_RETRY_WAIT);
        }
    }

    /// Arm `timer` to fire after `delay`, unless this process has armed its
    /// row already.
    fn spawn_scheduled_action(
        &self,
        timer: ScheduledTimer,
        delay: Duration,
        gate: Option<Arc<Semaphore>>,
    ) {
        let row = timer.row();
        {
            let mut armed = self
                .scheduled_armed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let live = armed.entry(row.clone()).or_insert(0);
            if *live > 0 {
                return;
            }
            *live += 1;
        }
        // The fire's own trace starts when it fires, linked to the move that
        // armed it (none for a row armed at boot).
        let armed_by = crate::trace_context::cause();
        let server = self.clone();
        let fire = async move {
            tokio::time::sleep(delay).await; // determinism-ok: scheduled delay
            let agent = &timer.agent_ctx;
            let span = crate::trace_context::runtime_work_span!(
                armed_by,
                "dispatch.scheduled_actions",
                workflow.root_entity_type = %agent.workflow_root_entity_type.as_deref().unwrap_or(&timer.entity_type),
                workflow.root_entity_id = %agent.workflow_root_entity_id.as_deref().unwrap_or(&timer.entity_id),
                workflow.run_id = %agent.workflow_run_id.clone()
                    .unwrap_or_else(|| format!("{}:{}", timer.entity_type, timer.entity_id)),
                temper.action = %timer.action,
                entity_type = %timer.entity_type,
                entity_id = %timer.entity_id,
            );
            server
                .fire_scheduled_action(&timer, gate)
                .instrument(span)
                .await;
            let mut armed = server
                .scheduled_armed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = armed.get_mut(&row) {
                *live -= 1;
                if *live == 0 {
                    armed.remove(&row);
                }
            }
        };
        tokio::spawn(fire); // determinism-ok: timer delivery is a background side-effect
    }

    /// Fire `timer` until its action commits or is refused, each attempt
    /// through `gate` when one is given. A refusal is logged and its row
    /// removed; a commit removed the row itself.
    async fn fire_scheduled_action(&self, timer: &ScheduledTimer, gate: Option<Arc<Semaphore>>) {
        let key = format!("schedule:{}", timer.arm_id);
        let mut agent = timer.agent_ctx.clone();
        agent.idempotency_key = Some(key.clone());
        agent.fires_scheduled_action = Some(timer.row());
        let mut wait = FIRST_RETRY_WAIT;
        let mut retried = false;
        loop {
            let outcome = {
                let _permit = match &gate {
                    Some(gate) => gate.clone().acquire_owned().await.ok(),
                    None => None,
                };
                self.dispatch_tenant_action(
                    &timer.tenant,
                    &timer.entity_type,
                    &timer.entity_id,
                    &timer.action,
                    serde_json::json!({}),
                    &agent,
                )
                .await
            };
            let error = match outcome {
                Ok(response) if response.success => {
                    if retried {
                        self.arm_fired_rows_retrying(timer, &key).await;
                    }
                    return;
                }
                Ok(response) if response.refused => {
                    let error = response
                        .error
                        .unwrap_or_else(|| "guard not met".to_string());
                    tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                        entity_id = %timer.entity_id, action = %timer.action,
                        arm_id = %timer.arm_id, %error, "scheduled action refused when it fired");
                    self.forget_scheduled_action(
                        &timer.tenant,
                        &timer.entity_type,
                        &timer.entity_id,
                        &timer.arm_id,
                    )
                    .await;
                    return;
                }
                Ok(response) => response.error.unwrap_or_else(|| "write failed".to_string()),
                Err(error) => error,
            };
            tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, action = %timer.action, arm_id = %timer.arm_id,
                %error, retry_in_ms = wait.as_millis() as u64,
                "scheduled action did not commit when it fired; retrying on its row");
            tokio::time::sleep(wait).await; // determinism-ok: retry backoff
            wait = (wait * 2).min(LAST_RETRY_WAIT);
            retried = true;
        }
    }

    /// Remove a refused scheduled action's row, or one that can never fire. A
    /// row left behind fires again at the next boot.
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
