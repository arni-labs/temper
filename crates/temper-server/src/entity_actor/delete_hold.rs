//! Holding an entity for a delete that removes it together with others, and
//! the delete itself.
//!
//! A server with no event journal keeps each entity only in its actor, so it
//! has no single write that removes several. A delete that cascades there
//! holds every entity it removes, each at the state it checked, and deletes
//! them once all are held (`ServerState::delete_entities_together`). While an
//! entity is held its writes are refused, so none of them can change between
//! the check and the delete. Each hold names the delete that holds it, so
//! only that delete's `DeleteHeld` or `Release` ends it (or the actor
//! stopping): a delete that gives up releases its own holds, never another's.

use super::actor::EntityActor;
use super::types::{EntityEvent, EntityMsg, EntityResponse, EntityState};

impl EntityActor {
    /// The refusal for `msg` while a delete holds this entity, or `None` when
    /// it may run: reads always run, and so do the hold's own messages.
    pub(super) fn refusal_while_held(
        &self,
        msg: &EntityMsg,
        state: &EntityState,
    ) -> Option<EntityResponse> {
        let held_by = self.holder()?;
        match msg {
            // A retry of the hold this entity is under, whose first answer
            // was lost.
            EntityMsg::Hold { holder, .. } if *holder == held_by => None,
            EntityMsg::Action { .. }
            | EntityMsg::UpdateFields { .. }
            | EntityMsg::Delete { .. }
            | EntityMsg::Hold { .. } => Some(reply(
                state,
                Err(
                    "a delete that removes this entity with others is under way; \
                     retry against current state"
                        .to_string(),
                ),
            )),
            EntityMsg::GetState
            | EntityMsg::GetField { .. }
            | EntityMsg::DeleteHeld { .. }
            | EntityMsg::Release { .. } => None,
        }
    }

    /// The delete holding this entity, if one is.
    fn holder(&self) -> Option<uuid::Uuid> {
        *self.held_by.lock().expect("hold lock poisoned")
    }

    fn set_holder(&self, holder: Option<uuid::Uuid>) {
        *self.held_by.lock().expect("hold lock poisoned") = holder;
    }

    /// Hold this entity for `holder` while its state still has `expected`,
    /// the digest of the state the delete checked. The caller has checked
    /// that no other delete holds it; a hold `holder` already has stands.
    pub(super) fn hold(
        &self,
        state: &EntityState,
        expected: &str,
        holder: uuid::Uuid,
    ) -> EntityResponse {
        if self.holder() == Some(holder) {
            return reply(state, Ok(()));
        }
        debug_assert!(self.holder().is_none());
        // A hold promises a delete, so an entity the delete would refuse is
        // not held, and the batch refuses before it deletes anything.
        if self.strict() {
            return reply(state, Err(STRICT_DELETE.to_owned()));
        }
        if state.status == "Deleted" {
            return reply(state, Err("entity is already deleted".to_string()));
        }
        if super::effects::entity_authorization_precondition(state) != expected {
            return reply(
                state,
                Err("entity changed after the delete checked it".to_string()),
            );
        }
        self.set_holder(Some(holder));
        reply(state, Ok(()))
    }

    /// End `holder`'s hold, if it holds this entity.
    pub(super) fn release(&self, state: &EntityState, holder: uuid::Uuid) -> EntityResponse {
        if self.holder() == Some(holder) {
            self.set_holder(None);
        }
        reply(state, Ok(()))
    }

    /// Delete this entity, which `holder` holds, and end the hold.
    pub(super) async fn delete_held(
        &self,
        state: &mut EntityState,
        holder: uuid::Uuid,
    ) -> EntityResponse {
        if self.holder() != Some(holder) {
            // A retry whose first attempt deleted the entity: while `holder`
            // held it, nothing else could.
            if state.status == "Deleted" {
                return reply(state, Ok(()));
            }
            return reply(
                state,
                Err("this delete does not hold the entity".to_string()),
            );
        }
        let response = self.delete_now(state, None).await;
        self.set_holder(None);
        response
    }

    /// Write this entity's tombstone, to the journal when there is one. With
    /// `expected`, only while the state still has that digest: the state an
    /// external authorization decision was made on.
    pub(super) async fn delete_now(
        &self,
        state: &mut EntityState,
        expected: Option<&str>,
    ) -> EntityResponse {
        if self.strict() {
            return reply(state, Err(STRICT_DELETE.to_owned()));
        }
        if let Some(expected) = expected
            && super::effects::entity_authorization_precondition(state) != expected
        {
            return reply(
                state,
                Err("delete authorization became stale; retry against current state".to_string()),
            );
        }
        let deleted = EntityEvent::tombstone(state.status.clone());
        if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            && let Err(e) = self
                .persist_event(store, backend, &self.persistence_id(), state, &deleted)
                .await
        {
            return super::actor::persistence_failure_reply(state.clone(), &e);
        }
        state.record_tombstone(deleted);
        reply(state, Ok(()))
    }
}

impl EntityActor {
    /// Whether only a declared action may delete this entity.
    fn strict(&self) -> bool {
        self.table
            .read()
            .expect("table lock poisoned")
            .strict_action_params
    }
}

const STRICT_DELETE: &str = "Strict entities require a declared action for deletion";

/// The reply to a delete-side message, with the entity's state after it. An
/// `Err` is a refusal: nothing was written.
fn reply(state: &EntityState, outcome: Result<(), String>) -> EntityResponse {
    match outcome {
        Ok(()) => EntityResponse {
            success: true,
            refusal: None,
            state: state.clone(),
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
            refused: false,
        },
        Err(error) => EntityResponse::refused(state.clone(), error),
    }
}
