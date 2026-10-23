//! Keeps the authz engine's group memberships current (ADR-0183).
//!
//! Every committed change of a `GroupMembership` (an action, a field update or
//! a delete) is applied as it happens, and a tenant's records are all read once
//! before the tenant can be found in a group: at boot, and before the first
//! check an identity reaches. Until then the engine answers "not a member" for
//! that tenant, so a missed read fails closed.

use temper_authz::groups::{
    ACTIVE_STATUS, GROUP_MEMBERSHIP_TYPE, MembershipRecord, MembershipSkip,
};
use temper_runtime::tenant::TenantId;

use super::ServerState;
use crate::entity_actor::EntityState;

impl ServerState {
    /// Apply one committed state of a `GroupMembership` record; any other type is ignored.
    pub(crate) fn note_group_membership(&self, tenant: &TenantId, state: &EntityState) {
        if state.entity_type != GROUP_MEMBERSHIP_TYPE {
            return;
        }
        let field = |key: &str| {
            state
                .fields
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
        };
        let record = MembershipRecord {
            tenant: tenant.as_str(),
            record_id: &state.entity_id,
            group: field("group"),
            member: field("member"),
            active: state.status == ACTIVE_STATUS,
            sequence_nr: state.sequence_nr,
        };
        if let Err(MembershipSkip::IdMismatch) = self.authz.group_memberships().apply(&record) {
            tracing::warn!(
                tenant = %tenant,
                entity_id = %state.entity_id,
                "GroupMembership id is not sha256(group, member); it makes nobody a member"
            );
        }
    }

    /// A `GroupMembership` record was deleted: it makes nobody a member any more.
    pub(crate) fn note_group_membership_deleted(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) {
        if entity_type == GROUP_MEMBERSHIP_TYPE {
            self.authz
                .group_memberships()
                .withdraw(tenant.as_str(), entity_id, sequence_nr);
        }
    }

    /// Read every `GroupMembership` of `tenant` once, then let the engine answer
    /// for the tenant. A tenant without the type has none, and nothing is kept for it.
    ///
    /// # Errors
    ///
    /// A record that cannot be read: the tenant stays unfilled, so nobody is
    /// found in a group there (fail closed), and the next call tries again.
    pub async fn fill_group_memberships(&self, tenant: &TenantId) -> Result<(), String> {
        let memberships = self.authz.group_memberships();
        if memberships.is_filled(tenant.as_str()) {
            return Ok(());
        }
        let governed = self
            .registry
            .read()
            .map_err(|_| "registry lock poisoned".to_string())?
            .get_table(tenant, GROUP_MEMBERSHIP_TYPE)
            .is_some();
        // A tenant without the type holds no memberships, and is left out of the
        // set: the tenant can be any name an unauthenticated caller sends.
        if !governed {
            return Ok(());
        }
        self.ensure_type_indexed(tenant, GROUP_MEMBERSHIP_TYPE)
            .await?;
        for id in self.list_entity_ids(tenant, GROUP_MEMBERSHIP_TYPE) {
            let response = self
                .get_tenant_entity_state(tenant, GROUP_MEMBERSHIP_TYPE, &id)
                .await
                .map_err(|error| format!("reading GroupMembership '{id}' failed: {error}"))?;
            self.note_group_membership(tenant, &response.state);
        }
        memberships.mark_filled(tenant.as_str());
        Ok(())
    }
}
