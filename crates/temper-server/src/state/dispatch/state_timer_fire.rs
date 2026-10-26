//! A timer's task owns its row and gate until its write is settled.
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tracing::Instrument;

use super::DispatchError;
use super::state_timeouts::ArmedTimer;
use crate::state::ServerState;

impl ServerState {
    /// Wait, fire through the normal dispatch pipeline, and remove the old
    /// row only after its durable successor is armed. A failed write keeps
    /// this arming (including its identity and journal fence) and retries.
    pub(super) fn spawn_state_timer(
        &self,
        timer: ArmedTimer,
        mut delay: Duration,
        gate: Option<Arc<Semaphore>>,
        armed_seq: u64,
    ) {
        self.state_timeout_tracker.inc_pending(&timer.entity_type);
        let workflow_root_entity_type = timer
            .agent_ctx
            .workflow_root_entity_type
            .clone()
            .unwrap_or_else(|| timer.entity_type.clone());
        let workflow_root_entity_id = timer
            .agent_ctx
            .workflow_root_entity_id
            .clone()
            .unwrap_or_else(|| timer.entity_id.clone());
        let workflow_run_id = timer
            .agent_ctx
            .workflow_run_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", timer.entity_type, timer.entity_id));

        tracing::debug!(
            tenant = %timer.tenant,
            entity_type = %timer.entity_type,
            entity_id = %timer.entity_id,
            target_state = %timer.state,
            target_action = %timer.action,
            delay_ms = delay.as_millis() as u64,
            workflow.root_entity_type = %workflow_root_entity_type,
            workflow.root_entity_id = %workflow_root_entity_id,
            workflow.run_id = %workflow_run_id,
            "armed state timeout"
        );

        let state = self.clone();
        let armed_by = crate::trace_context::cause();
        tokio::spawn(async move {
            // determinism-ok: timer work on the runtime's scheduler
            loop {
                tokio::time::sleep(delay).await; // determinism-ok: simulated runtime clock
                let permit = match &gate {
                    Some(gate) => gate.clone().acquire_owned().await.ok(),
                    None => None,
                };
                let span = crate::trace_context::runtime_work_span!(
                    armed_by,
                    "dispatch.state_timeout.fire",
                    tenant = %timer.tenant,
                    entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id,
                    target_state = %timer.state,
                    target_action = %timer.action,
                    workflow.root_entity_type = %workflow_root_entity_type,
                    workflow.root_entity_id = %workflow_root_entity_id,
                    workflow.run_id = %workflow_run_id,
                );
                let retry = state
                    .fire_state_timer(&timer, armed_seq)
                    .instrument(span)
                    .await;
                // The dispatch awaited the store answer and ran the normal
                // post-commit pipeline. No fire write survives this permit.
                drop(permit);
                let Some((wait, reason)) = retry else { break };
                if !state.state_timer_is_current(&timer, armed_seq) {
                    break;
                }
                tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id, arm_id = %timer.arm_id,
                    %reason, retry_in_ms = wait.as_millis() as u64,
                    "state timer retained after failed fire; retrying");
                delay = wait;
            }
            state.forget_state_timer(&timer).await;
            state.state_timeout_tracker.dec_pending(&timer.entity_type);
        });
    }

    /// None when this arming is finished; otherwise the retry delay and reason.
    async fn fire_state_timer(
        &self,
        timer: &ArmedTimer,
        armed_seq: u64,
    ) -> Option<(Duration, String)> {
        if !self.state_timer_is_current(timer, armed_seq) {
            return None;
        }
        // Use the dispatch retry policy's final backoff for a store failure,
        // not a fresh full state budget or a zero-delay overdue spin.
        let policy = self.dispatch_retry_policy();
        let retry_delay =
            Duration::from_millis(policy.base_delay_ms_for(policy.max_attempts).max(1));
        let current = match self
            .get_tenant_entity_state(&timer.tenant, &timer.entity_type, &timer.entity_id)
            .await
        {
            Ok(current) => current.state,
            Err(reason) => return Some((retry_delay, reason)),
        };
        let belongs = current.status == timer.state && self.declares_state_timer(timer);
        if !belongs {
            self.reconcile_state_timers(
                &timer.tenant,
                &timer.entity_type,
                &timer.entity_id,
                &current,
                Some(&timer.agent_ctx),
            )
            .await;
        } else if current.sequence_nr > timer.sequence_nr {
            self.follow_moved_record(timer, armed_seq, &current).await;
        } else if !self.state_timer_already_fired(timer, &current) {
            crate::runtime_metrics::record_state_timeout_fired(
                timer.tenant.as_str(),
                &timer.entity_type,
                &timer.state,
                &timer.action,
            );
            let agent_ctx = timer.fire_context();
            let answer = self
                .dispatch_tenant_action_ext_typed(
                    &timer.tenant,
                    &timer.entity_type,
                    &timer.entity_id,
                    &timer.action,
                    timer.params.clone(),
                    super::DispatchExtOptions {
                        agent_ctx: &agent_ctx,
                        await_integration: false,
                        await_reactions: true,
                    },
                )
                .await;
            match answer {
                Ok(answer) if !answer.success && answer.state.sequence_nr > timer.sequence_nr => {
                    self.follow_moved_record(timer, armed_seq, &answer.state)
                        .await;
                }
                Ok(answer) if !answer.success && !answer.refused => {
                    return Some((
                        retry_delay,
                        answer.error.unwrap_or_else(|| "fire write failed".into()),
                    ));
                }
                Err(DispatchError::Deferred { retry_after_ms }) => {
                    return Some((
                        Duration::from_millis(retry_after_ms.max(1)),
                        "dispatch deferred by admission".into(),
                    ));
                }
                Err(error) => return Some((retry_delay, error.to_string())),
                _ => {}
            }
        }
        None
    }
}
