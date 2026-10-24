//! The rows a transition's scheduled actions are written with.
//!
//! A scheduled action (`schedule` or `schedule_at`) is a durable timer: its
//! row is written in the same commit as the event that armed it, so a restart
//! arms it again (`ServerState::rearm_scheduled_actions`). Its id is the
//! arming event's journal sequence and the action's place among the event's
//! schedules, `<seq>.<index>`; the fire's idempotency key derives from it, so
//! a fire repeated after a restart is a duplicate and never runs twice.

use temper_runtime::persistence::ScheduledActionRecord;

use super::actor::EntityActor;
use super::effects::ScheduledAction;
use super::types::{EntityEvent, EntityState};
use crate::request_context::AgentContext;
use crate::request_context::armed_by::encode_armed_by;

impl EntityActor {
    /// Name each scheduled action `event` arms and return the rows to write
    /// with it. `state` is the entity before the event is written. Without a
    /// journal nothing is kept and no row is returned; the ids stay unique in
    /// this process.
    pub(super) fn arm_scheduled_actions(
        &self,
        scheduled: &mut [ScheduledAction],
        state: &EntityState,
        event: &EntityEvent,
        caller: Option<&AgentContext>,
    ) -> Vec<ScheduledActionRecord> {
        let journaled = self.event_journal.is_some() && self.event_backend.is_some();
        let sequence = if journaled {
            state.sequence_nr + 1
        } else {
            state.total_event_count as u64
        };
        for (index, action) in scheduled.iter_mut().enumerate() {
            action.arm_id = format!("{sequence}.{index}");
        }
        if !journaled || scheduled.is_empty() {
            return Vec::new();
        }
        let armed_by =
            encode_armed_by(caller.unwrap_or(&AgentContext::default())).unwrap_or_else(|error| {
                tracing::error!(entity_type = %state.entity_type, entity_id = %state.entity_id,
                    %error, "scheduled action caller not encodable; it fires with no identity");
                "{}".to_string()
            });
        scheduled
            .iter()
            .map(|action| ScheduledActionRecord {
                tenant: self.tenant.clone(),
                entity_type: state.entity_type.clone(),
                entity_id: state.entity_id.clone(),
                arm_id: action.arm_id.clone(),
                action: action.action.clone(),
                due_at: i64::try_from(action.delay_seconds)
                    .ok()
                    .and_then(chrono::Duration::try_seconds)
                    .and_then(|delay| event.timestamp.checked_add_signed(delay))
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                    .to_rfc3339(),
                armed_by: armed_by.clone(),
            })
            .collect()
    }
}
