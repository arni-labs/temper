//! The journal streams an atomic composite batch appends to: one per record
//! it writes, opened before the batch stages its events.

use super::*;

/// A record an atomic composite batch writes.
pub(super) struct StreamTarget<'a> {
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    /// The record's state as the preflight read it, when it did.
    pub(super) preflight_target: Option<&'a PreflightCompositeTarget>,
    /// Whether the stream starts without a `Created` event (a parent-gated
    /// create), unless the record's spec has input contracts.
    pub(super) suppress_bootstrap_event: bool,
}

impl crate::state::ServerState {
    /// Open the journal stream of `target` for the batch, once: from its
    /// preflight state when the preflight read it, else from the record as it
    /// is. A record that does not exist yet starts with its `Created` event,
    /// made as `origin` records.
    pub(super) async fn ensure_atomic_composite_stream(
        &self,
        streams: &mut BTreeMap<String, AtomicCompositeStream>,
        tenant: &TenantId,
        target: StreamTarget<'_>,
        origin: &MoveOrigin,
    ) -> Result<(), DispatchError> {
        let StreamTarget {
            entity_type,
            entity_id,
            preflight_target,
            suppress_bootstrap_event,
        } = target;
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        if streams.contains_key(&persistence_id) {
            return Ok(());
        }

        let (target_exists, mut state) = if let Some(target) = preflight_target {
            (target.target_existed, target.state.clone())
        } else {
            let table = self.transition_table_for_dispatch(tenant, entity_type)?;
            let target_exists = self
                .ensure_entity_loaded(tenant, entity_type, entity_id)
                .await;
            let state = if target_exists {
                self.get_tenant_entity_state(tenant, entity_type, entity_id)
                    .await
                    .map_err(DispatchError::Internal)?
                    .state
            } else {
                synthetic_initial_state(entity_type, entity_id, &table)
            };
            (target_exists, state)
        };
        let expected_sequence = state.sequence_nr;
        let mut events = Vec::new();
        let retain_bootstrap = !suppress_bootstrap_event
            || self
                .transition_table_for_dispatch(tenant, entity_type)?
                .has_input_contracts();
        if retain_bootstrap
            && !target_exists
            && expected_sequence == 0
            && state.total_event_count == 0
        {
            let bootstrap = EntityEvent::bootstrap(state.status.clone(), origin);
            events.push(composite_envelope(&persistence_id, &bootstrap, &state)?);
            state.sequence_nr = state.sequence_nr.saturating_add(1);
            state.push_event_bounded(bootstrap);
        }
        streams.insert(
            persistence_id,
            AtomicCompositeStream {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                target_existed: target_exists,
                state,
                expected_sequence,
                events,
            },
        );
        Ok(())
    }
}
