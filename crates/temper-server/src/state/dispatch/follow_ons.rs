//! A committed move's follow-ons, held in this process until they settle.
//!
//! A move that owes follow-ons writes their row in its commit
//! (`request_context::follow_ons`). In this process each follow-on holds the
//! row from the moment it starts until it settles: the dispatch that sends
//! them, a reaction or child or integration sent in the background, and an
//! owed write while it is tried again (`owed_writes`). The context they are
//! sent in names the row ([`AgentContext::follow_ons`]). When the last hold
//! settles the row is removed, tried again while the store fails. A follow-on
//! that never settles, because the process stopped, leaves the row.
//!
//! At boot ([`ServerState::resume_follow_ons`]) every row a previous process
//! left is sent again, as whoever made the move, under the move's key: its
//! reactions, its children and its integrations' calls. Each derives the key
//! it was first sent under, so one that had landed is answered as a
//! duplicate, and the row is removed once they settle here. A module call
//! runs again; the call's answer is keyed the same, so it applies once.
//!
//! A row this process holds is never sent again by a boot listing retried
//! while it serves: its follow-ons are in flight here.

use std::sync::{Arc, PoisonError};

use temper_runtime::persistence::ScheduledActionRecord;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;

use super::owed_writes::{FIRST_RETRY_WAIT, LAST_RETRY_WAIT};
use crate::request_context::follow_ons::{OwedFollowOns, follow_arm_id};
use crate::request_context::{AgentContext, ScheduledFire};
use crate::state::ServerState;

/// The follow-on row of the move committed on a record under `key`.
pub(crate) fn follow_row(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    key: &str,
) -> ScheduledFire {
    ScheduledFire {
        tenant: tenant.as_str().to_string(),
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        arm_id: follow_arm_id(key),
    }
}

/// One follow-on's hold on its move's row, if its context names one. Settle
/// it ([`ServerState::settle_follow_on`]) once the follow-on has landed, was
/// refused, or failed for good; one dropped unsettled keeps the row for the
/// next boot.
#[must_use = "a hold that is never settled keeps its row until the next boot"]
pub(crate) struct FollowOnHold(Option<ScheduledFire>);

impl ServerState {
    /// Hold the row `agent_ctx` names for a follow-on that starts now.
    pub(crate) fn hold_follow_on(&self, agent_ctx: &AgentContext) -> FollowOnHold {
        if let Some(row) = &agent_ctx.follow_ons {
            self.hold_follow_row(row);
        }
        FollowOnHold(agent_ctx.follow_ons.clone())
    }

    /// Settle `hold`; the last hold on its row removes the row.
    pub(crate) fn settle_follow_on(&self, hold: FollowOnHold) {
        if let Some(row) = hold.0 {
            self.settle_follow_row(&row);
        }
    }

    /// Hold `row` once more.
    pub(super) fn hold_follow_row(&self, row: &ScheduledFire) {
        *self
            .follow_ons_held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(row.clone())
            .or_insert(0) += 1;
    }

    /// Settle one hold on `row`, and remove the row if it was the last.
    pub(super) fn settle_follow_row(&self, row: &ScheduledFire) {
        let settled = {
            let mut held = self
                .follow_ons_held
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match held.get_mut(row) {
                Some(holds) if *holds > 1 => {
                    *holds -= 1;
                    false
                }
                Some(_) => {
                    held.remove(row);
                    true
                }
                None => {
                    debug_assert!(false, "a follow-on row settled more often than held");
                    false
                }
            }
        };
        if settled {
            let server = self.clone();
            let row = row.clone();
            tokio::spawn(async move { server.remove_follow_row(&row).await }); // determinism-ok: settled row cleanup
        }
    }

    /// Whether this process holds `row`: its follow-ons are in flight here.
    fn holds_follow_row(&self, row: &ScheduledFire) -> bool {
        self.follow_ons_held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(row)
    }

    /// Remove a settled row, trying again with the owed-write waits while the
    /// store fails. A row whose removal never lands is sent again at the next
    /// boot, where each follow-on is answered as a duplicate.
    async fn remove_follow_row(&self, row: &ScheduledFire) {
        let Some((store, _)) = self.event_journal() else {
            return;
        };
        let mut wait = FIRST_RETRY_WAIT;
        loop {
            match store
                .delete_scheduled_action(&row.tenant, &row.entity_type, &row.entity_id, &row.arm_id)
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(tenant = %row.tenant, entity_type = %row.entity_type,
                        entity_id = %row.entity_id, arm_id = %row.arm_id, %error,
                        retry_in_ms = wait.as_millis() as u64,
                        "settled follow-on row not removed; trying again");
                    tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                    wait = (wait * 2).min(LAST_RETRY_WAIT);
                }
            }
        }
    }

    /// Send again, through `gate`, the follow-ons a previous process left in
    /// `row`, unless this process holds it. A row that cannot be read is
    /// removed: nothing could send what it owes.
    pub(super) fn resume_follow_ons(
        &self,
        tenant: &TenantId,
        row: &ScheduledActionRecord,
        gate: &Arc<Semaphore>,
    ) {
        let fire = ScheduledFire {
            tenant: tenant.as_str().to_string(),
            entity_type: row.entity_type.clone(),
            entity_id: row.entity_id.clone(),
            arm_id: row.arm_id.clone(),
        };
        if self.holds_follow_row(&fire) {
            return;
        }
        let read = serde_json::from_str::<OwedFollowOns>(&row.armed_by)
            .and_then(|owed| owed.caller().map(|caller| (owed, caller)));
        let (owed, mut caller) = match read {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                    entity_id = %row.entity_id, arm_id = %row.arm_id, %error,
                    "follow-on row not readable at boot; it is removed");
                self.hold_follow_row(&fire);
                self.settle_follow_row(&fire);
                return;
            }
        };
        caller.follow_ons = Some(fire.clone());
        // The resume holds the row as the dispatch that first sent them did.
        self.hold_follow_row(&fire);
        let server = self.clone();
        let tenant = tenant.clone();
        let action = row.action.clone();
        // The row is due at the move's event.
        let moved_at = chrono::DateTime::parse_from_rfc3339(&row.due_at)
            .ok()
            .map(|at| at.with_timezone(&chrono::Utc));
        let gate = gate.clone();
        let resume = async move {
            let _permit = gate.acquire_owned().await.ok();
            if server
                .send_follow_ons(&tenant, &fire, &action, moved_at, &owed, &caller)
                .await
            {
                server.settle_follow_row(&fire);
            }
        };
        tokio::spawn(resume); // determinism-ok: boot recovery of owed follow-ons
    }

    /// Send the follow-ons `owed` names for the move `action`, made at
    /// `moved_at`, on `fire`'s record, as `caller`: its reactions, children and integrations, each
    /// holding the row while in flight. False when its integrations could not
    /// be sent because the record could not be read: the row is kept for the
    /// next boot.
    async fn send_follow_ons(
        &self,
        tenant: &TenantId,
        fire: &ScheduledFire,
        action: &str,
        moved_at: Option<chrono::DateTime<chrono::Utc>>,
        owed: &OwedFollowOns,
        caller: &AgentContext,
    ) -> bool {
        tracing::info!(tenant = %tenant, entity_type = %fire.entity_type,
            entity_id = %fire.entity_id, action, reactions = owed.reactions.len(),
            children = owed.spawn_requests.len(), effects = owed.custom_effects.len(),
            "sending the follow-ons a previous process left owed");
        if !owed.reactions.is_empty() {
            let moved = crate::trigger::dispatcher::CommittedMove {
                entity_type: &fire.entity_type,
                entity_id: &fire.entity_id,
                action,
                to_state: &owed.status,
                fields: &owed.fields,
            };
            self.dispatch_declared_reactions(tenant, &moved, Some(&owed.reactions), caller)
                .await;
        }
        if !owed.spawn_requests.is_empty() {
            self.dispatch_spawn_requests(
                tenant,
                &fire.entity_type,
                &fire.entity_id,
                &owed.spawn_requests,
                &owed.params,
                caller,
            );
        }
        if owed.custom_effects.is_empty() {
            return true;
        }
        // The integrations read the record as the move left it, at the
        // move's sequence, which names its calls' deadline rows, and at its
        // time, from which their tries and deadlines count.
        let mut entity_state = match self
            .get_tenant_entity_state(tenant, &fire.entity_type, &fire.entity_id)
            .await
        {
            Ok(response) => response.state,
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type = %fire.entity_type,
                    entity_id = %fire.entity_id, %error,
                    "record of owed integrations not readable; its row is kept for the next boot");
                return false;
            }
        };
        entity_state.status = owed.status.clone();
        entity_state.fields = owed.fields.clone();
        entity_state.sequence_nr = owed.sequence;
        if moved_at.is_some() {
            entity_state.last_event.last_event_at = moved_at;
        }
        self.dispatch_wasm_integrations(
            tenant,
            &fire.entity_type,
            &fire.entity_id,
            action,
            &owed.custom_effects,
            &entity_state,
            caller,
            &owed.params,
        );
        self.dispatch_adapter_integrations(super::adapter::AdapterDispatchInput {
            tenant,
            entity_type: &fire.entity_type,
            entity_id: &fire.entity_id,
            action,
            custom_effects: &owed.custom_effects,
            entity_state: &entity_state,
            agent_ctx: caller,
            action_params: &owed.params,
        });
        true
    }
}
