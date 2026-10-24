//! Walks that make every record of a timed type follow its state's timer.
//!
//! An install that changes a type's spec, or gives a type its first timeout,
//! can leave records in a timed state with the timer of another state or
//! none. A boot can find records in a timed state with no timer row (an
//! install walk the last process did not finish, a row write that failed).
//! Both walk the type's records and reconcile each one
//! ([`ServerState::reconcile_state_timers`]), so every record in a timed state
//! ends with its timer armed, or waits for a caller (see
//! [`TimerFollow::WaitsForCaller`]).
//!
//! A record in memory is reconciled with its actor's state. One that is not is
//! replayed from the store without starting an actor, so a walk over many
//! records does not leave them all resident.

use std::collections::BTreeMap;
use std::time::Duration;

use temper_runtime::tenant::TenantId;

use super::state_timer_reconcile::TimerFollow;
use crate::entity_actor::{EntityState, recover_entity_state_from_store};
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::projection_backfill::transition_table_for;

/// The wait before listing a type's records again after the store failed to
/// list them. It doubles up to [`FOLLOW_RETRY_MAX`], so a store that stays
/// down is asked about once a minute, not in a tight loop.
const FOLLOW_RETRY_FIRST: Duration = Duration::from_secs(1);
const FOLLOW_RETRY_MAX: Duration = Duration::from_secs(60);

/// Who armed the timer of a record whose row the boot dropped (its state no
/// longer declares a timeout), so the record's new timer fires as them.
pub(super) type BootCallers = BTreeMap<(TenantId, String, String), AgentContext>;

impl ServerState {
    /// Make the timers of every record of each type whose spec an install
    /// changed follow the state the record is in now, including a type that
    /// had no timeout before. Each changed type is followed once.
    pub async fn follow_spec_changes(&self) {
        let changed = match self.registry.write() {
            Ok(mut registry) => registry.take_changed_types(),
            Err(_) => return,
        };
        self.follow_types(changed, &BootCallers::new()).await;
    }

    /// [`Self::follow_spec_changes`] in the background, for an install that
    /// answers before every record is read.
    pub fn spawn_follow_spec_changes(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let server = self.clone();
        runtime.spawn(async move { server.follow_spec_changes().await }); // determinism-ok: install follow-up runs in the background
    }

    /// At boot, after the rows are re-armed: make every record of every type
    /// that declares a timeout follow its state, as an install would.
    pub(super) async fn follow_timed_types(&self, callers: BootCallers) {
        let types: Vec<(TenantId, String)> = match self.registry.read() {
            Ok(registry) => registry
                .tenant_ids()
                .into_iter()
                .flat_map(|tenant| {
                    registry
                        .entity_types(tenant)
                        .into_iter()
                        .filter(|entity_type| {
                            registry
                                .get_spec(tenant, entity_type)
                                .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
                        })
                        .map(|entity_type| (tenant.clone(), entity_type.to_string()))
                        .collect::<Vec<_>>()
                })
                .collect(),
            Err(_) => return,
        };
        self.follow_types(types, &callers).await;
    }

    /// Reconcile every record of each of `types`. A type whose records the
    /// store cannot list is tried again after a wait, since its records not
    /// in memory would otherwise keep the timers of their old state.
    async fn follow_types(&self, mut types: Vec<(TenantId, String)>, callers: &BootCallers) {
        let mut wait = FOLLOW_RETRY_FIRST;
        loop {
            let mut unlisted = Vec::new();
            for (tenant, entity_type) in types {
                if let Err(error) = self.ensure_type_indexed(&tenant, &entity_type).await {
                    tracing::warn!(tenant = %tenant, entity_type, %error,
                        retry_in_secs = wait.as_secs(),
                        "records of a timed type could not be listed; trying again");
                    unlisted.push((tenant, entity_type));
                    continue;
                }
                let ids = self.list_entity_ids(&tenant, &entity_type);
                let mut waiting = 0usize;
                for entity_id in &ids {
                    let key = (tenant.clone(), entity_type.clone(), entity_id.clone());
                    let followed = self
                        .reconcile_stored_record(
                            &tenant,
                            &entity_type,
                            entity_id,
                            callers.get(&key),
                        )
                        .await;
                    waiting += usize::from(followed == TimerFollow::WaitsForCaller);
                }
                tracing::info!(tenant = %tenant, entity_type, records = ids.len(), waiting,
                    "state timers follow the records' states");
                if waiting > 0 {
                    tracing::warn!(tenant = %tenant, entity_type, waiting,
                        "records in a timed state wait for their next action: the timeout action binds its caller and nobody armed it");
                }
            }
            if unlisted.is_empty() {
                return;
            }
            tokio::time::sleep(wait).await; // determinism-ok: retry wait on the runtime clock
            wait = (wait * 2).min(FOLLOW_RETRY_MAX);
            types = unlisted;
        }
    }

    /// Reconcile one record: with its actor's state when it is in memory,
    /// else with its state replayed from the store. A record that cannot be
    /// read keeps its timer; that is checked again when the timer fires.
    pub(super) async fn reconcile_stored_record(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        armed_by: Option<&AgentContext>,
    ) -> TimerFollow {
        let Some(current) = self
            .current_record_state(tenant, entity_type, entity_id)
            .await
        else {
            return TimerFollow::Settled;
        };
        if current.total_event_count == 0 {
            return TimerFollow::Settled;
        }
        self.reconcile_state_timers(tenant, entity_type, entity_id, &current, armed_by)
            .await
    }

    /// The record's state now, without starting its actor when it has none.
    async fn current_record_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<EntityState> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let resident = self
            .actor_registry
            .read()
            .is_ok_and(|actors| actors.get(&key).is_some_and(|actor| !actor.is_closed()));
        let journal = self.event_journal();
        let (Some((store, backend)), false) = (journal, resident) else {
            return match self
                .get_tenant_entity_state(tenant, entity_type, entity_id)
                .await
            {
                Ok(response) => Some(response.state),
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "record not readable; its timer is not reconciled");
                    None
                }
            };
        };
        let table = transition_table_for(self, tenant, entity_type)?;
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        match recover_entity_state_from_store(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            blob_store.as_ref(),
            true,
        )
        .await
        {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                    "record not replayable; its timer is not reconciled");
                None
            }
        }
    }
}
