//! Removing several entities together: all of them, or none.
//!
//! A delete that cascades removes the requested entity and the rows that
//! belong to it, each as it was read and checked. With an event journal their
//! tombstones are appended in one `append_batch`, each conditional on the
//! journal sequence its state was read at, so either every one is removed or,
//! when any changed after it was read, none is. No reader of the journal sees
//! part of the set removed, so the set needs no removal order, and a cycle of
//! references inside it (a row that references itself, two rows that
//! reference each other) needs none either. After the write each entity's
//! actor is stopped, so the next read replays the tombstone, and its query
//! projection row is removed. A projection removal that fails leaves a row
//! whose journal has moved past it, which the delete reference lookup does
//! not trust (`delete_references`).
//!
//! Without a journal each entity lives only in its actor. Every member is
//! held at the state it was checked in (`EntityMsg::Hold`), which refuses its
//! writes; a member that changed refuses the hold, and every hold is released
//! with nothing deleted. Once all are held, each is deleted.
//!
//! The work runs in its own task, so a request that goes away part way (its
//! client disconnected) cannot leave members held, or live actors serving the
//! state from before a committed tombstone.

use temper_runtime::persistence::{
    EventMetadata, PersistenceAppend, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use tracing::{Instrument, instrument};

use super::ServerState;
use crate::entity_actor::effects::entity_authorization_precondition;
use crate::entity_actor::{EntityEvent, EntityMsg, EntityResponse, EntityState};

/// Why a batch delete removed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchDeleteError {
    /// An entity changed after its state was read.
    Changed(String),
    /// The store or an actor could not take the delete.
    Storage(String),
}

impl ServerState {
    /// Remove every entity in `members` together: only while each one is
    /// still in the state given for it, and then all of them. Each member is
    /// a distinct, live entity of `tenant`.
    #[instrument(skip_all, fields(otel.name = "entity.delete_entities_together", tenant = %tenant, members = members.len()))]
    pub(crate) async fn delete_entities_together(
        &self,
        tenant: &TenantId,
        members: Vec<EntityState>,
    ) -> Result<(), BatchDeleteError> {
        debug_assert!(
            {
                let mut ids = std::collections::BTreeSet::new();
                members
                    .iter()
                    .all(|member| ids.insert((&member.entity_type, &member.entity_id)))
            },
            "batch delete members must be distinct"
        );
        let state = self.clone();
        let tenant = tenant.clone();
        // determinism-ok: the task only outlives a request that goes away; it
        // is awaited here, so the delete's order of effects does not change.
        tokio::spawn(
            async move {
                if state.event_journal().is_some() {
                    state.append_tombstones_together(&tenant, members).await
                } else {
                    state.delete_held_together(&tenant, members).await
                }
            }
            .instrument(tracing::Span::current()),
        )
        .await
        .map_err(|error| BatchDeleteError::Storage(format!("delete task failed: {error}")))?
    }

    /// One `append_batch` of every member's tombstone, each conditional on the
    /// sequence its state was read at.
    async fn append_tombstones_together(
        &self,
        tenant: &TenantId,
        members: Vec<EntityState>,
    ) -> Result<(), BatchDeleteError> {
        let Some((store, _backend)) = self.event_journal() else {
            return Err(BatchDeleteError::Storage(
                "the event journal went away".to_string(),
            ));
        };
        let mut appends = Vec::with_capacity(members.len());
        let mut deleted = Vec::with_capacity(members.len());
        for mut member in members {
            debug_assert!(member.sequence_nr > 0, "a journaled entity has events");
            let persistence_id = format!("{tenant}:{}:{}", member.entity_type, member.entity_id);
            let tombstone = EntityEvent::tombstone(member.status.clone());
            let payload = crate::entity_actor::bootstrap::event_payload(&tombstone, &member)
                .map_err(|error| BatchDeleteError::Storage(error.to_string()))?;
            appends.push(PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: member.sequence_nr,
                events: vec![PersistenceEnvelope {
                    sequence_nr: member.sequence_nr + 1,
                    event_type: tombstone.action.clone(),
                    payload,
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: tombstone.timestamp,
                        actor_id: persistence_id,
                    },
                }],
            });
            member.record_tombstone(tombstone);
            member.sequence_nr += 1;
            deleted.push(member);
        }

        match store.append_batch(&appends).await {
            Ok(results) => debug_assert_eq!(results.len(), deleted.len()),
            Err(PersistenceError::ConcurrencyViolation { expected, actual }) => {
                return Err(BatchDeleteError::Changed(format!(
                    "an entity it removes changed after it was read (journal at {actual}, read at {expected})"
                )));
            }
            Err(error) => return Err(BatchDeleteError::Storage(error.to_string())),
        }

        // The tombstones are the journal's truth from here on. Each live
        // actor still holds the state from before them, so it goes first.
        for member in &deleted {
            self.cache_entity_status(tenant, member);
            self.stop_and_remove_entity(tenant, &member.entity_type, &member.entity_id);
        }
        for member in &deleted {
            self.remove_deleted_entity_projection(
                tenant,
                &member.entity_type,
                &member.entity_id,
                member.sequence_nr,
            )
            .await;
        }
        Ok(())
    }

    /// Hold every member at the state it was read in, then delete each.
    async fn delete_held_together(
        &self,
        tenant: &TenantId,
        members: Vec<EntityState>,
    ) -> Result<(), BatchDeleteError> {
        let holder = sim_uuid();
        let mut actors = Vec::with_capacity(members.len());
        for member in &members {
            let Some(actor) =
                self.get_or_spawn_tenant_actor(tenant, &member.entity_type, &member.entity_id)
            else {
                release(&actors, holder);
                return Err(BatchDeleteError::Storage(format!(
                    "no transition table for {}",
                    member.entity_type
                )));
            };
            let held = actor
                .ask::<EntityResponse>(
                    EntityMsg::Hold {
                        expected_authorization_precondition: entity_authorization_precondition(
                            member,
                        ),
                        holder,
                    },
                    self.action_dispatch_timeout,
                )
                .await;
            // Sent a hold, so it may hold even when its answer did not arrive.
            actors.push(actor);
            match held {
                Ok(response) if response.success => {}
                Ok(response) => {
                    release(&actors, holder);
                    return Err(BatchDeleteError::Changed(format!(
                        "{}('{}'): {}",
                        member.entity_type,
                        member.entity_id,
                        response.error.unwrap_or_default()
                    )));
                }
                Err(error) => {
                    release(&actors, holder);
                    return Err(BatchDeleteError::Storage(format!(
                        "could not hold {}('{}'): {error}",
                        member.entity_type, member.entity_id
                    )));
                }
            }
        }

        // Every member is held at the state it was checked in, so each
        // delete below finds that state.
        for (actor, member) in actors.iter().zip(&members) {
            let deleted = actor
                .ask::<EntityResponse>(
                    EntityMsg::DeleteHeld { holder },
                    self.action_dispatch_timeout,
                )
                .await;
            match deleted {
                Ok(response) if response.success => {}
                Ok(response) => {
                    return Err(BatchDeleteError::Storage(format!(
                        "{}('{}') was held but not deleted: {}",
                        member.entity_type,
                        member.entity_id,
                        response.error.unwrap_or_default()
                    )));
                }
                Err(error) => {
                    return Err(BatchDeleteError::Storage(format!(
                        "{}('{}') was held but not deleted: {error}",
                        member.entity_type, member.entity_id
                    )));
                }
            }
            // Nothing keeps a tombstone without a journal, so the status
            // cache entry goes with the entity, as a single delete does.
            if let Ok(mut cache) = self.entity_state_cache.lock() {
                cache.pop(&format!(
                    "{tenant}:{}:{}",
                    member.entity_type, member.entity_id
                ));
            }
            self.stop_and_remove_entity(tenant, &member.entity_type, &member.entity_id);
        }
        Ok(())
    }
}

/// End `holder`'s holds on every actor a hold was sent to; a release reaches
/// an actor after its hold, and leaves another delete's hold alone.
fn release(actors: &[temper_runtime::actor::ActorRef<EntityMsg>], holder: uuid::Uuid) {
    for actor in actors {
        let _ = actor.tell(EntityMsg::Release { holder });
    }
}

#[cfg(test)]
#[path = "batch_delete_test.rs"]
mod tests;
