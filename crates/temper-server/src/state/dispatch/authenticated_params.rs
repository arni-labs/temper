//! Resolve spec-declared trusted inputs before action validation and persistence.
use serde_json::Value;
use temper_authz::PrincipalKind;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::ParameterSource;

use super::DispatchError;
use crate::{ServerState, request_context::AgentContext};

impl ServerState {
    /// Bind declared parameters from authenticated authority. The caller's JSON
    /// never establishes identity, including when it supplies the correct value.
    pub(crate) fn resolve_authenticated_params(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        mut params: Value,
        agent: &AgentContext,
    ) -> Result<Value, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let action = action.rsplit('.').next().unwrap_or(action);
        let Some(contract) = table.action_contracts.get(action) else {
            return Ok(params);
        };
        if contract.param_sources.is_empty() {
            return Ok(params);
        }
        let fields = params.as_object_mut().ok_or_else(|| {
            DispatchError::Internal("Action parameters must be a JSON object".into())
        })?;
        for (name, source) in &contract.param_sources {
            if fields.contains_key(name) {
                return Err(DispatchError::AuthzDenied(format!(
                    "Parameter '{name}' is bound by the runtime and cannot be supplied"
                )));
            }
            let context = agent.security_ctx.as_ref().ok_or_else(|| {
                DispatchError::AuthzDenied(
                    "Authenticated parameter requires a security context".into(),
                )
            })?;
            let principal = &context.principal;
            if principal.id.is_empty()
                || principal.id == "anonymous"
                || principal.kind == PrincipalKind::System
                || principal.role.as_deref() == Some("service")
            {
                return Err(DispatchError::AuthzDenied(
                    "Authenticated subject requires a caller identity".into(),
                ));
            }
            let value = match source {
                ParameterSource::AuthenticatedSubject => {
                    principal.acting_for.as_deref().unwrap_or(&principal.id)
                }
            };
            if value.is_empty() {
                return Err(DispatchError::AuthzDenied(
                    "Authenticated subject is empty".into(),
                ));
            }
            fields.insert(name.clone(), Value::String(value.to_owned()));
        }
        Ok(params)
    }
}
