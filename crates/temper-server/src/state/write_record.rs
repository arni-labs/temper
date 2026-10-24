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

use futures_util::FutureExt;
use serde_json::Value;
use temper_authz::SecurityContext;
use temper_jit::table::{RelatedMap, TransitionTable};
use temper_runtime::tenant::TenantId;
use tracing::instrument::WithSubscriber;

use super::ServerState;
use crate::entity_actor::effects::{self, FieldSyncMode, SpawnRequest};
use crate::entity_actor::{EntityActor, EntityState};
use crate::odata::constraints::{self, ConstraintViolation, PendingRecords};

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

/// One write about to be applied: who makes it and what it stores into.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordWrite<'a> {
    /// Where the write came from.
    pub caller: WriteCaller,
    /// The tenant the record belongs to.
    pub tenant: &'a TenantId,
    /// The record's entity type.
    pub entity_type: &'a str,
    /// The record's id.
    pub entity_id: &'a str,
    /// The action, which selects the cross-invariants that apply (their `on`
    /// trigger), for example "Create" or "Patch".
    pub action: &'a str,
    /// The write's label in violations and metrics, for example "create".
    pub operation: &'a str,
}

/// An action about to be sent to the actor, with what it reads to build the
/// record it will store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActionPreview<'a> {
    /// The entity type's transition table.
    pub table: &'a TransitionTable,
    /// The record before the action.
    pub current: &'a EntityState,
    /// The action's name.
    pub action: &'a str,
    /// The action's params.
    pub params: &'a Value,
    /// Related records the action's guards read.
    pub related: &'a RelatedMap,
}

impl ServerState {
    /// Check the record a write will store against the tenant's relation
    /// integrity, field invariants and hard cross-invariants. A kernel
    /// follow-on is not checked (see the module docs).
    pub(crate) async fn check_record_to_store(
        &self,
        write: RecordWrite<'_>,
        record: &Value,
    ) -> Result<(), ConstraintViolation> {
        self.check_record_with_pending(write, record, &PendingRecords::default())
            .await
    }

    /// [`Self::check_record_to_store`], with relation integrity and
    /// cross-invariants reading each record in `pending`, which the write
    /// brings into being after it commits, as it will then stand.
    async fn check_record_with_pending(
        &self,
        write: RecordWrite<'_>,
        record: &Value,
        pending: &PendingRecords,
    ) -> Result<(), ConstraintViolation> {
        let RecordWrite {
            caller,
            tenant,
            entity_type,
            entity_id,
            operation,
            ..
        } = write;
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
            pending,
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
        constraints::post_write_invariant_checks(self, write, record, pending).await
    }

    /// Check the record `action` will store when applied to `current`.
    ///
    /// Runs before the action is sent to the actor, which the caller then
    /// holds to `current`: the actor applies the action only while the record
    /// is still `current`, so the record checked here is the record stored.
    /// Kernel follow-ons and entity types with no relation, field invariant
    /// or matching cross-invariant skip the work.
    pub(crate) async fn check_action_record(
        &self,
        caller: WriteCaller,
        tenant: &TenantId,
        preview: ActionPreview<'_>,
    ) -> Result<(), ConstraintViolation> {
        let ActionPreview {
            table,
            current,
            action,
            params,
            related,
        } = preview;
        if !self.action_record_checked(caller, tenant, &current.entity_type, action) {
            return Ok(());
        }
        // Boxed as a trait object: the preview runs the action interpreter,
        // and its future type nested in every dispatch path is too deep for
        // their callers' `Send` checks.
        let Some((record, pending)) = self
            .action_record(tenant, table, current, action, params, related)
            .boxed()
            .await
        else {
            // The interpreter refused on `current`, so the action stores
            // nothing there, and the actor applies it to no other state.
            return Ok(());
        };
        self.check_record_with_pending(
            RecordWrite {
                caller,
                tenant,
                entity_type: &current.entity_type,
                entity_id: &current.entity_id,
                action,
                operation: "action",
            },
            &record,
            &pending,
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
        let registry = self.registry.read().expect("registry lock poisoned");
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

    /// The fields `action` stores when applied to `current`, and the
    /// children its spawns create once it commits, or `None` when the
    /// interpreter refuses the action on that state.
    async fn action_record(
        &self,
        tenant: &TenantId,
        table: &TransitionTable,
        current: &EntityState,
        action: &str,
        params: &Value,
        related: &RelatedMap,
    ) -> Option<(Value, PendingRecords)> {
        let (record, spawns) = self
            .preview_action(tenant, table, current.clone(), action, params, related)
            .await?;
        let mut pending = PendingRecords::default();
        for spawn in &spawns {
            if let Some(child) = self.pending_child(tenant, current, spawn, params).await {
                pending.insert(child);
            }
        }
        Some((record.fields, pending))
    }

    /// `state` after `action`, with the spawns the action requests: the
    /// actor's own action interpreter run on the copy, or `None` when the
    /// interpreter refuses the action on that state.
    async fn preview_action(
        &self,
        tenant: &TenantId,
        table: &TransitionTable,
        mut state: EntityState,
        action: &str,
        params: &Value,
        related: &RelatedMap,
    ) -> Option<(EntityState, Vec<SpawnRequest>)> {
        // The preview repeats the interpreter's own log lines; only the real
        // application of the action should appear in the logs.
        let result = crate::entity_actor::action_input::process_action_with_blob_prestate(
            &mut state,
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
        result.success.then_some((state, result.spawn_requests))
    }

    /// The child `spawn` creates once `parent`'s action (with `params`)
    /// commits, as it will stand after its initial action, from the same plan
    /// the kernel creates it by. `None` when the kernel creates no child for
    /// the spawn.
    async fn pending_child(
        &self,
        tenant: &TenantId,
        parent: &EntityState,
        spawn: &SpawnRequest,
        params: &Value,
    ) -> Option<EntityState> {
        let plan = self
            .spawn_plan(
                tenant,
                &parent.entity_type,
                &parent.entity_id,
                spawn,
                params,
            )
            .await
            .ok()?;
        let child = if self
            .ensure_entity_loaded(tenant, &spawn.entity_type, &spawn.entity_id)
            .await
        {
            self.get_tenant_entity_state(tenant, &spawn.entity_type, &spawn.entity_id)
                .await
                .ok()?
                .state
        } else {
            EntityActor::build_initial_state(
                &spawn.entity_type,
                &spawn.entity_id,
                &plan.table,
                &plan.initial_fields,
            )
        };
        let Some((action, action_params)) = &plan.initializer else {
            return Some(child);
        };
        let related = self
            .resolve_guards_from_fields(tenant, &spawn.entity_type, action, &child.fields)
            .await;
        let initialized = self
            .preview_action(
                tenant,
                &plan.table,
                child.clone(),
                action,
                action_params,
                &related,
            )
            .await;
        // A refused initial action leaves the child in its first state.
        Some(initialized.map_or(child, |(initialized, _)| initialized))
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
