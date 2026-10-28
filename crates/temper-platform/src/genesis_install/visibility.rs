//! The rule every anonymous registry read shares: the repository is public.
//!
//! The bundle route and the compare route both answer callers who present no
//! credential. Each checks visibility here, before it reads any of a
//! repository's content.

use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::state::ServerState;

use super::GenesisAppBundle;

/// Refuse unless this repository is public.
///
/// Called for each app as the closure is walked, BEFORE its tree is
/// materialized. An app's bundle is its whole dependency closure, so "is this
/// public?" is a question about every repository in it — and the check has to
/// happen before the content is read, not after: resolving the closure
/// materializes each tree in order to read its manifest, so a check that ran
/// afterwards would already have written a private repository to disk.
pub(super) async fn refuse_unless_repository_is_public(
    state: &ServerState,
    tenant: &TenantId,
    app: &GenesisAppBundle,
) -> Result<(), String> {
    if !state
        .ensure_entity_loaded(tenant, "Repository", &app.repository_id)
        .await
    {
        return Err(format!(
            "Genesis bundle refused: repository {} for {}/{} could not be read",
            app.repository_id, app.owner, app.name
        ));
    }
    let repository = state
        .get_tenant_entity_state(tenant, "Repository", &app.repository_id)
        .await
        .map_err(|error| {
            format!(
                "Genesis bundle refused: repository {} could not be read: {error}",
                app.repository_id
            )
        })?;
    let visibility = repository
        .state
        .fields
        .get("Visibility")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if visibility != "public" {
        tracing::warn!(
            tenant = %tenant,
            repository_id = %app.repository_id,
            app = %app.name,
            visibility,
            "anonymous Genesis bundle refused: a repository in the closure is not public"
        );
        return Err(format!(
            "Genesis bundle refused: {}/{} is not public",
            app.owner, app.name
        ));
    }
    Ok(())
}
