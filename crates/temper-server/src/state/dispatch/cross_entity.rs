use crate::request_context::AgentContext;
use temper_jit::table::{Related, RelatedMap};
use temper_runtime::tenant::TenantId;
use tracing::Instrument;

impl crate::state::ServerState {
    /// Resolve the related entities a move of `action` reads: its guards, and
    /// the type's invariants, which every move is checked against.
    ///
    /// For each `Type[id_field].status` they read, reads the
    /// id (or list of ids) from stored fields or declared defaults and resolves each
    /// related entity's status. An unset reference resolves to no statuses;
    /// an entity that is not found resolves to `None`. Every id is resolved:
    /// the number of lookups is the number of ids the entity's own fields
    /// hold, so a guard over all of a list's entities sees all of them.
    pub(super) async fn resolve_related_statuses(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> RelatedMap {
        let mut result = RelatedMap::new();
        let table = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            let Some(spec) = registry.get_spec(tenant, entity_type) else {
                return result;
            };
            spec.table()
        };
        let refs = table.related_refs(action);
        if refs.is_empty() {
            return result;
        }

        // Get current entity fields to resolve target entity IDs
        let current_fields = match self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(resp) => resp.state.fields,
            Err(_) => return result,
        };

        for (target_type, id_field) in refs {
            let ids = crate::entity_actor::related::reference_ids(
                &current_fields,
                &table.initial_values,
                &id_field,
            );
            let mut statuses = Vec::with_capacity(ids.len());
            for id in ids {
                statuses.push(self.resolve_entity_status(tenant, &target_type, id).await);
            }
            result.insert((target_type, id_field), Related::Statuses(statuses));
        }
        result
    }

    /// Dispatch entity spawn requests post-transition.
    ///
    /// This is a **sync** method (like `arm_scheduled_actions`) so that
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
            let child_type = req.entity_type.clone();
            let child_id = req.entity_id.clone();
            let initial_action = req.initial_action.clone();
            let parent_params = action_params.clone();
            let copied_fields = req.copied_fields.clone();
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
            let span = crate::trace_context::runtime_work_span!(
                crate::trace_context::cause(),
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
                    let mut parent_fields = serde_json::Map::new();
                    parent_fields.insert(
                        "parent_type".to_string(),
                        serde_json::Value::String(parent_t.clone()),
                    );
                    parent_fields.insert(
                        "parent_id".to_string(),
                        serde_json::Value::String(parent_i.clone()),
                    );
                    parent_fields.insert(
                        format!("{}_id", to_snake_case(&parent_t)),
                        serde_json::Value::String(parent_i.clone()),
                    );
                    let child_table = match state.transition_table_for_dispatch(&t, &child_type) {
                        Ok(table) => table,
                        Err(error) => {
                            state.record_generated_callback_refusal(
                                super::WasmEntityRef {
                                    tenant: &t,
                                    entity_type: &parent_t,
                                    entity_id: &parent_i,
                                },
                                "spawn",
                                &error.to_string(),
                            );
                            return;
                        }
                    };
                    let strict_child = child_table.strict_action_params;
                    if strict_child && initial_action.is_none() {
                        state.record_generated_callback_refusal(
                            super::WasmEntityRef {
                                tenant: &t,
                                entity_type: &parent_t,
                                entity_id: &parent_i,
                            },
                            "spawn",
                            "Strict child requires a declared initializer for its generated fields",
                        );
                        return;
                    }
                    let initializer = if let Some(action) = initial_action {
                        // Copied parent fields fill in what this action's own
                        // params do not carry; a param wins, so a value the
                        // spawning action just set is never overwritten by the
                        // field as it stood before the action.
                        let mut params = copied_fields;
                        params.extend(parent_params.as_object().cloned().unwrap_or_default());
                        params.extend(parent_fields.clone());
                        match state.prepare_generated_action_params(
                            &t,
                            &child_type,
                            &action,
                            serde_json::Value::Object(params),
                        ) {
                            Ok(params) => Some((action, params)),
                            Err(error) => {
                                state.record_generated_callback_refusal(
                                    super::WasmEntityRef {
                                        tenant: &t,
                                        entity_type: &parent_t,
                                        entity_id: &parent_i,
                                    },
                                    &action,
                                    &error,
                                );
                                return;
                            }
                        }
                    } else {
                        None
                    };
                    // A strict child's declared initializer accepts its data. The
                    // spawn effect supplies only identity to generic creation.
                    let initial_fields = if strict_child {
                        serde_json::json!({})
                    } else {
                        serde_json::Value::Object(parent_fields.clone())
                    };

                    if let Some((action, params)) = &initializer {
                        let table = &child_table;
                        let validation = state
                            .load_authz_resource_snapshot(&t, &child_type, &child_id)
                            .await
                            .and_then(|snapshot| {
                                // Existing actors validate against their hydrated blob
                                // values and execution-time state. Only absent children
                                // need this check before creation can persist anything.
                                if snapshot.exists {
                                    return Ok(());
                                }
                                let prestate =
                                    crate::entity_actor::EntityActor::build_initial_state(
                                        &child_type,
                                        &child_id,
                                        table,
                                        &initial_fields,
                                    );
                                table
                                    .validate_action_params(
                                        action,
                                        params,
                                        &prestate.fields,
                                        &prestate.counters,
                                        &prestate.booleans,
                                    )
                                    .map_err(String::from)
                            });
                        if let Err(error) = validation {
                            state.record_generated_callback_refusal(
                                super::WasmEntityRef {
                                    tenant: &t,
                                    entity_type: &parent_t,
                                    entity_id: &parent_i,
                                },
                                action,
                                &error,
                            );
                            return;
                        }
                    }

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

fn to_snake_case(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        match ch {
            'A'..='Z' => {
                if index > 0 {
                    result.push('_');
                }
                result.push(ch.to_ascii_lowercase());
            }
            '-' | ' ' => result.push('_'),
            _ => result.push(ch.to_ascii_lowercase()),
        }
    }
    result
}

#[cfg(test)]
#[path = "cross_entity_test.rs"]
mod required_ref_tests;
