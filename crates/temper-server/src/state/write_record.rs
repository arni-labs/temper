//! The record a write will store, and the one constraint check every write
//! path runs on it.
//!
//! Relation integrity, field invariants and hard cross-invariants describe a
//! stored record. Each write path builds the record it is about to store with
//! the same function the entity actor applies for that write (create, field
//! update, action) and checks that record here, so the record that is checked
//! is the record that is stored.
//!
//! Only writes from outside the kernel are checked. The kernel's own
//! follow-ons for a spec (reactions, state timers, schedules, WASM callbacks,
//! a WASM module's requests back into the API, platform code) must be able to
//! finish what an earlier move started, such as ending a session whose parent
//! was archived since.

use serde_json::Value;
use temper_authz::SecurityContext;
use temper_jit::table::{RelatedMap, TransitionTable};
use temper_runtime::tenant::TenantId;
use tracing::instrument::WithSubscriber;

use super::ServerState;
use crate::entity_actor::effects::{self, FieldSyncMode};
use crate::entity_actor::{EntityActor, EntityState};
use crate::odata::constraints::{self, ConstraintViolation};

/// Where a write came from. It decides whether the record the write stores
/// is checked before it is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteCaller {
    /// A call from outside the kernel: OData and MCP requests, HttpEndpoint
    /// routes and inbound webhooks.
    Outside,
    /// A follow-on the kernel runs for the spec: reactions, state timers,
    /// schedules, WASM callbacks and platform code, including a WASM module's
    /// own requests back into the kernel's API.
    Kernel,
}

impl WriteCaller {
    /// The caller behind an authenticated API request. A request a WASM
    /// module makes back into the kernel carries `context.module`, which the
    /// kernel sets when it hands the module its credential and which no
    /// outside credential resolves to; that request is the module's own work
    /// for the spec. Every other request comes from outside.
    pub(crate) fn of_request(security_ctx: &SecurityContext) -> Self {
        if security_ctx.context_attrs.contains_key("module") {
            Self::Kernel
        } else {
            Self::Outside
        }
    }
}

impl ServerState {
    /// Check the record a write will store against the tenant's relation
    /// integrity, field invariants and hard cross-invariants. A kernel
    /// follow-on is not checked (see the module docs).
    ///
    /// `action` selects which cross-invariants apply (their `on` trigger);
    /// `operation` labels the write in violations and metrics.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn check_record_to_store(
        &self,
        caller: WriteCaller,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        operation: &str,
        record: &Value,
    ) -> Result<(), ConstraintViolation> {
        if caller == WriteCaller::Kernel {
            return Ok(());
        }
        constraints::pre_upsert_relation_checks(
            self,
            tenant,
            entity_type,
            entity_id,
            operation,
            record,
        )
        .await?;
        constraints::pre_upsert_field_invariant_checks(
            self,
            tenant,
            entity_type,
            entity_id,
            operation,
            record,
        )
        .await?;
        constraints::post_write_invariant_checks(
            self,
            tenant,
            entity_type,
            entity_id,
            action,
            record,
            operation,
        )
        .await
    }

    /// Check the record `action` will store when applied to `current`.
    ///
    /// Runs before the action is sent to the actor. Kernel follow-ons and
    /// entity types with no relation, field invariant or matching
    /// cross-invariant skip the work.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn check_action_record(
        &self,
        caller: WriteCaller,
        tenant: &TenantId,
        table: &TransitionTable,
        current: &EntityState,
        action: &str,
        params: &Value,
        related: &RelatedMap,
    ) -> Result<(), ConstraintViolation> {
        if !self.action_record_checked(caller, tenant, &current.entity_type, action) {
            return Ok(());
        }
        let record = self
            .action_record(tenant, table, current, action, params, related)
            .await;
        self.check_record_to_store(
            caller,
            tenant,
            &current.entity_type,
            &current.entity_id,
            action,
            "action",
            &record,
        )
        .await
    }

    /// Whether the record a `caller`'s `action` on `entity_type` stores is
    /// checked: the call comes from outside the kernel and the tenant declares
    /// a constraint that write must satisfy.
    pub(crate) fn action_record_checked(
        &self,
        caller: WriteCaller,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> bool {
        if caller == WriteCaller::Kernel || !self.cross_invariant_enforce {
            return false;
        }
        let registry = self.registry.read().unwrap(); // ci-ok: RwLock read — poisoned lock = prior panic, fail-fast correct
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return false;
        };
        let has_relations = tenant_config
            .relation_graph
            .outgoing
            .get(entity_type)
            .is_some_and(|edges| !edges.is_empty());
        let has_cross_invariants = tenant_config.cross_invariants.as_ref().is_some_and(|spec| {
            spec.invariants
                .iter()
                .any(|inv| constraints::trigger_matches(&inv.on, entity_type, action))
        });
        let has_field_invariants = registry
            .field_invariants_for(tenant, entity_type)
            .is_some_and(|invariants| !invariants.is_empty());
        has_relations || has_cross_invariants || has_field_invariants
    }

    /// The fields `action` stores when applied to `current`: the actor's own
    /// action interpreter run on a copy of the state.
    async fn action_record(
        &self,
        tenant: &TenantId,
        table: &TransitionTable,
        current: &EntityState,
        action: &str,
        params: &Value,
        related: &RelatedMap,
    ) -> Value {
        let mut preview = current.clone();
        // The preview repeats the interpreter's own log lines; only the real
        // application of the action should appear in the logs.
        let result = crate::entity_actor::action_input::process_action_with_blob_prestate(
            &mut preview,
            table,
            action,
            params,
            related,
            FieldSyncMode::blob_refs_default(),
            crate::blobs::BlobReadSource::Tenant {
                state: self,
                tenant,
            },
        )
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
        if !result.success {
            // The interpreter refused on this state. The actor normally refuses
            // too and stores nothing; if the state moved on in between, the
            // action's params written over the current fields is what it can
            // store.
            return params_written_over(current, params);
        }
        let mut fields = preview.fields;
        if let Some(fields) = fields.as_object_mut() {
            // A spawn writes the id of a child the kernel creates after this
            // commit; that field is checked as it stood before the action.
            for field in result
                .spawn_requests
                .iter()
                .filter_map(|spawn| spawn.store_id_in.as_deref())
            {
                match current.fields.get(field) {
                    Some(previous) => {
                        fields.insert(field.to_string(), previous.clone());
                    }
                    None => {
                        fields.remove(field);
                    }
                }
            }
        }
        fields
    }
}

/// The fields a create stores: the initial fields as the actor builds its
/// first state from them.
pub(crate) fn created_record(
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    initial_fields: &Value,
) -> Value {
    EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields).fields
}

/// The fields a field update stores: a PATCH merge (`replace == false`) or a
/// PUT replacement, applied to `current` by the actor's own field-update
/// function.
pub(crate) fn field_update_record(current: &EntityState, body: &Value, replace: bool) -> Value {
    let body = effects::sanitize_action_params(body);
    let mut next = current.clone();
    if effects::apply_field_update(&mut next, &body, replace) {
        next.fields
    } else {
        current.fields.clone()
    }
}

/// `current`'s fields with the action's params written over them, as the
/// actor projects params into fields.
fn params_written_over(current: &EntityState, params: &Value) -> Value {
    field_update_record(current, params, false)
}
