use crate::request_context::AgentContext;
use temper_jit::table::{Related, RelatedMap};
use temper_runtime::tenant::TenantId;
use tracing::Instrument;

impl crate::state::ServerState {
    /// Resolve the related entities an action's guards read.
    ///
    /// For each `Type[id_field].status` in the guards of `action`, reads the
    /// id (or list of ids) from the entity's current fields and resolves each
    /// related entity's status. An unset reference resolves to no statuses;
    /// an entity that is not found resolves to `None`. Every id is resolved:
    /// the number of lookups is the number of ids the entity's own fields
    /// hold, so a guard over all of a list's entities sees all of them.
    pub(super) async fn resolve_cross_entity_guards(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> RelatedMap {
        let refs = self.guard_related_refs(tenant, entity_type, action);
        if refs.is_empty() {
            return RelatedMap::new();
        }

        // Get current entity fields to resolve target entity IDs
        let current_fields = match self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(resp) => resp.state.fields,
            Err(_) => return RelatedMap::new(),
        };
        self.resolve_guard_refs(tenant, refs, &current_fields, None)
            .await
    }

    /// The `(related type, id field)` pairs the guards of `action` read.
    pub(super) fn guard_related_refs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> Vec<(String, String)> {
        let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
        registry
            .get_spec(tenant, entity_type)
            .map(|spec| spec.table().guard_related_refs(action))
            .unwrap_or_default()
    }

    /// Resolve each related entity `refs` names through the ids in
    /// `current_fields`. A related entity that is `committed` is read as that
    /// state instead of the store.
    pub(super) async fn resolve_guard_refs(
        &self,
        tenant: &TenantId,
        refs: Vec<(String, String)>,
        current_fields: &serde_json::Value,
        committed: Option<&crate::entity_actor::EntityState>,
    ) -> RelatedMap {
        let mut result = RelatedMap::new();
        for (target_type, id_field) in refs {
            let ids: Vec<&str> = match current_fields.get(&id_field) {
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .filter(|id| !id.is_empty())
                    .collect(),
                Some(serde_json::Value::String(id)) if !id.is_empty() => vec![id.as_str()],
                _ => Vec::new(),
            };
            let mut statuses = Vec::with_capacity(ids.len());
            for id in ids {
                let status = match committed {
                    Some(record) if record.entity_type == target_type && record.entity_id == id => {
                        Some(record.status.clone())
                    }
                    _ => self.resolve_entity_status(tenant, &target_type, id).await,
                };
                statuses.push(status);
            }
            result.insert((target_type, id_field), Related::Statuses(statuses));
        }
        result
    }

    /// Dispatch entity spawn requests post-transition.
    ///
    /// This is a **sync** method (like `dispatch_scheduled_actions`) so that
    /// `tokio::spawn` inside it does not cause async recursion.
    /// Creates child entities and optionally dispatches initial actions.
    ///
    /// Every request is dispatched. Each one comes from a `spawn(...)` effect
    /// written in the spec, so their number is fixed by the spec text.
    pub(super) fn dispatch_spawn_requests(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        spawn_requests: &[crate::entity_actor::effects::SpawnRequest],
        action_params: &serde_json::Value,
        agent_ctx: &AgentContext,
    ) {
        for req in spawn_requests {
            let state = self.clone();
            let t = tenant.clone();
            let parent_t = parent_type.to_string();
            let parent_i = parent_id.to_string();
            let req = req.clone();
            let child_type = req.entity_type.clone();
            let child_id = req.entity_id.clone();
            let parent_params = action_params.clone();
            let agent = agent_ctx.clone();
            let workflow_root_entity_type = agent
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| parent_t.clone());
            let workflow_root_entity_id = agent
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| parent_i.clone());
            let workflow_run_id = agent
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{parent_t}:{parent_i}"));
            let span = tracing::info_span!(
                "dispatch.background_spawn_entity",
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                parent_type = %parent_t,
                parent_id = %parent_i,
                child_type = %child_type,
                child_id = %child_id,
            );

            tokio::spawn(
                async move {
                    // determinism-ok: spawn dispatch is a background side-effect
                    let plan = match state
                        .spawn_plan(&t, &parent_t, &parent_i, &req, &parent_params)
                        .await
                    {
                        Ok(plan) => plan,
                        Err(refusal) => {
                            state.record_generated_callback_refusal(
                                super::WasmEntityRef {
                                    tenant: &t,
                                    entity_type: &parent_t,
                                    entity_id: &parent_i,
                                },
                                &refusal.action,
                                &refusal.error,
                            );
                            return;
                        }
                    };
                    let super::spawn_plan::SpawnPlan {
                        initial_fields,
                        initializer,
                        ..
                    } = plan;

                    match state
                        .get_or_create_tenant_entity(&t, &child_type, &child_id, initial_fields)
                        .await
                    {
                        Ok(created) => {
                            tracing::info!(
                                parent_type = %parent_t,
                                parent_id = %parent_i,
                                child_type = %child_type,
                                child_id = %child_id,
                                "spawned child entity"
                            );

                            // A child with no initial action stays in its
                            // first state: arm that state's timer here.
                            if initializer.is_none() {
                                state
                                    .arm_state_timeouts_on_creation(
                                        &t,
                                        &child_type,
                                        &child_id,
                                        &created,
                                        &agent,
                                    )
                                    .await;
                            }
                            if let Some((action, params)) = initializer {
                                let result = state
                                    .dispatch_tenant_action(
                                        &t,
                                        &child_type,
                                        &child_id,
                                        &action,
                                        params,
                                        &agent,
                                    )
                                    .await;
                                let refusal = match result {
                                    Err(error) => Some(error),
                                    Ok(response) if !response.success => {
                                        Some(response.error.unwrap_or_else(|| {
                                            "child initializer refused".to_string()
                                        }))
                                    }
                                    Ok(_) => None,
                                };
                                if let Some(error) = refusal {
                                    // The child stays in its first state: arm
                                    // that state's timer, as for no initializer.
                                    state
                                        .arm_state_timeouts_on_creation(
                                            &t,
                                            &child_type,
                                            &child_id,
                                            &created,
                                            &agent,
                                        )
                                        .await;
                                    state.record_generated_callback_refusal(
                                        super::WasmEntityRef {
                                            tenant: &t,
                                            entity_type: &parent_t,
                                            entity_id: &parent_i,
                                        },
                                        &action,
                                        &error,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                child_type = %child_type,
                                child_id = %child_id,
                                error = %e,
                                "failed to spawn child entity"
                            );
                        }
                    }
                }
                .instrument(span),
            );
        }
    }
}

#[cfg(test)]
#[path = "cross_entity_test.rs"]
mod required_ref_tests;
