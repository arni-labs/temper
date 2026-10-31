//! The rows a transition's scheduled actions are written with.
//!
//! A scheduled action (`schedule` or `schedule_at`) is a durable timer: its
//! row is written in the same commit as the event that armed it, so a restart
//! arms it again (`ServerState::rearm_scheduled_actions`). Its id is the
//! arming event's journal sequence and the action's place among the event's
//! schedules, `<seq>.<index>`. The event a fire writes removes the fired
//! row in its own commit, so a row never outlives its fire and a restart
//! cannot fire it again.

use temper_runtime::persistence::ScheduledActionRecord;

use super::actor::EntityActor;
use super::effects::ScheduledAction;
use super::types::{EntityEvent, EntityState};
use crate::request_context::AgentContext;
use crate::request_context::armed_by::encode_scheduled_armed_by;

/// The scheduled-action rows an event changes in its commit.
#[derive(Debug, Default)]
pub(super) struct ScheduledRows {
    /// The rows of the scheduled actions it arms.
    pub(super) armed: Vec<ScheduledActionRecord>,
    /// The arm id of this record's scheduled action it fires.
    pub(super) fired: Option<String>,
}

impl EntityActor {
    /// Name each scheduled action `event` arms, and return the rows to write
    /// with it and the row it fires, if `caller` is firing one of this
    /// record's. `state` is the entity before the event is written. Without a
    /// journal nothing is kept and no row is returned; the ids stay unique in
    /// this process.
    pub(super) fn arm_scheduled_actions(
        &self,
        scheduled: &mut [ScheduledAction],
        state: &EntityState,
        event: &EntityEvent,
        caller: Option<&AgentContext>,
    ) -> ScheduledRows {
        let journaled = self.event_journal.is_some() && self.event_backend.is_some();
        let sequence = if journaled {
            state.sequence_nr + 1
        } else {
            state.total_event_count as u64
        };
        for (index, action) in scheduled.iter_mut().enumerate() {
            action.arm_id = format!("{sequence}.{index}");
            // An absolute schedule keeps its target, even if processing the
            // arming event took time. A relative schedule starts at the event.
            action.due_at = action.due_at.or_else(|| {
                i64::try_from(action.delay_millis)
                    .ok()
                    .and_then(chrono::Duration::try_milliseconds)
                    .and_then(|delay| event.timestamp.checked_add_signed(delay))
            });
        }
        if !journaled {
            return ScheduledRows::default();
        }
        let fired = caller
            .and_then(|caller| caller.fires_scheduled_action.as_ref())
            .filter(|fire| {
                fire.tenant == self.tenant
                    && fire.entity_type == state.entity_type
                    && fire.entity_id == state.entity_id
            })
            .map(|fire| fire.arm_id.clone());
        if scheduled.is_empty() {
            return ScheduledRows {
                armed: Vec::new(),
                fired,
            };
        }
        let armed = scheduled
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let follow_on = caller
                    .unwrap_or(&AgentContext::default())
                    .for_scheduled_follow_on(&action.action, index, action.delay_millis);
                let armed_by = encode_scheduled_armed_by(&follow_on).unwrap_or_else(|error| {
                    tracing::error!(entity_type = %state.entity_type, entity_id = %state.entity_id,
                        %error, "scheduled action caller not encodable; it fires with no identity");
                    "{}".to_string()
                });
                ScheduledActionRecord {
                    tenant: self.tenant.clone(),
                    entity_type: state.entity_type.clone(),
                    entity_id: state.entity_id.clone(),
                    arm_id: action.arm_id.clone(),
                    action: action.action.clone(),
                    due_at: action
                        .due_at
                        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                        .to_rfc3339(),
                    armed_by,
                }
            })
            .collect();
        ScheduledRows { armed, fired }
    }
}
