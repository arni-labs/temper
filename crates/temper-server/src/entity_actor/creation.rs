//! A record whose journal is empty is not written yet. Its `Created` event
//! commits with its first write, in that write's transaction, so a process
//! that stops between two commits cannot leave the record created without
//! the write that created it. A read of the record writes it alone: a record
//! that is read exists, as it always has.

use temper_runtime::actor::ActorError;

use super::EntityActor;
use super::effects::sanitize_action_params;
use super::scheduled_rows::ScheduledRows;
use super::{EntityEvent, EntityState};

/// Whether `event` is a record's `Created` event, the first in its journal.
pub(super) fn is_creation(event: &EntityEvent) -> bool {
    event.action == "Created" && event.from_status.is_empty()
}

impl EntityActor {
    /// The `Created` event of a record not written yet, and the record it
    /// creates (the event's payload carries its initial values). None once
    /// its journal holds an event, or with no journal.
    pub(super) fn unwritten_creation(
        &self,
        state: &EntityState,
    ) -> Option<(EntityEvent, EntityState)> {
        if self.event_journal.is_none() || state.sequence_nr != 0 {
            return None;
        }
        let table = self.table.read().expect("table lock poisoned").clone();
        let initial = Self::build_initial_state(
            &self.entity_type,
            &self.entity_id,
            &table,
            &self.initial_fields,
        );
        let mut created = EntityEvent::bootstrap(initial.status.clone());
        created.params = sanitize_action_params(&self.initial_fields).into_owned();
        Some((created, initial))
    }

    /// Write the `Created` event of a record not written yet, alone, as its
    /// creator's: a first state with a timer commits the timer's row with it.
    pub(super) async fn write_creation(&self, state: &mut EntityState) -> Result<(), ActorError> {
        let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend) else {
            return Ok(());
        };
        let Some((created, _)) = self.unwritten_creation(state) else {
            return Ok(());
        };
        self.persist_event(
            store,
            backend,
            state,
            &created,
            &ScheduledRows::default(),
            self.created_by.as_ref(),
        )
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to persist the Created event for {}:{}: {error}",
                self.entity_type, self.entity_id
            ))
        })?;
        state.push_event_bounded(created);
        Ok(())
    }
}
