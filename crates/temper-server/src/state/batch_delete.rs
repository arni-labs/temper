//! Removing several entities in one journal write.
//!
//! A delete that cascades removes the requested entity and the rows that
//! belong to it. Their tombstones are appended in one `append_batch`, each
//! conditional on the journal sequence its state was read at: either every
//! one of them is removed, or, when any of them changed after it was read,
//! none is. No reader of the journal sees part of the set removed, so the set
//! needs no removal order, and a cycle of references inside it (a row that
//! references itself, two rows that reference each other) needs none either.
//!
//! After the write each entity's actor is stopped, so the next read replays
//! the tombstone, and its query projection row is removed. A projection
//! removal that fails leaves a row whose journal has moved past it, which the
//! delete reference lookup does not trust (`delete_references`).

use temper_runtime::persistence::{
    EventMetadata, PersistenceAppend, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::ServerState;
use crate::entity_actor::{EntityEvent, EntityState};

/// Why a batch delete removed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchDeleteError {
    /// The server keeps no event journal, so it has no write that removes
    /// several entities at once.
    NoJournal,
    /// An entity changed after its state was read.
    Changed(String),
    /// The journal could not take the write.
    Storage(String),
}

impl ServerState {
    /// Remove every entity in `members` in one journal write, which commits
    /// only while each one's journal is still at the sequence of the state
    /// given for it. Each member is a distinct, live entity of `tenant`.
    #[instrument(skip_all, fields(otel.name = "entity.delete_entities_together", tenant = %tenant, members = members.len()))]
    pub(crate) async fn delete_entities_together(
        &self,
        tenant: &TenantId,
        members: Vec<EntityState>,
    ) -> Result<(), BatchDeleteError> {
        let Some((store, _backend)) = self.event_journal() else {
            return Err(BatchDeleteError::NoJournal);
        };
        debug_assert!(
            {
                let mut ids = std::collections::BTreeSet::new();
                members
                    .iter()
                    .all(|member| ids.insert((&member.entity_type, &member.entity_id)))
            },
            "batch delete members must be distinct"
        );

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
}

#[cfg(test)]
#[path = "batch_delete_test.rs"]
mod tests;
