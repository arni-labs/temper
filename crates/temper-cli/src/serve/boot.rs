//! What `temper serve` does between opening its store and serving.
//!
//! [`read_stored`] reads the specs the store holds, [`restore_own`] brings
//! the server's own tenants back, and [`start_serving`] starts the runtime
//! just before the listener answers.

use std::collections::BTreeMap;

use anyhow::Result;

use temper_platform::state::PlatformState;
use temper_server::registry::SpecRegistry;
use temper_server::storage::StorageStack;

use super::bootstrap;

/// What a boot read from its store before it serves.
pub(super) struct Stored {
    /// The registry with the tenants restored so far.
    pub registry: SpecRegistry,
    /// Cedar policy text of the `--app` tenants loaded from disk.
    pub policy_seed: BTreeMap<String, String>,
    /// The tenants restored after the server serves.
    pub later: Later,
}

/// The tenants a boot restores after it serves.
pub(super) struct Later;

/// Read the store's specs into a registry, and the `--app` specs from disk.
pub(super) async fn read_stored(
    pg_pool: Option<&sqlx::PgPool>,
    storage_stack: Option<&StorageStack>,
    apps: &[(String, String)],
) -> Result<Stored> {
    let (registry, policy_seed) = bootstrap::build_registry(pg_pool, storage_stack, apps).await?;
    Ok(Stored {
        registry,
        policy_seed,
        later: Later,
    })
}

/// Bring the server's own tenants back: the `--app` tenants' Cedar from disk
/// (`policy_seed`), their records, Cedar, modules, secrets, the system and
/// agent specs, and their installed apps.
pub(super) async fn restore_own(
    state: &PlatformState,
    policy_seed: BTreeMap<String, String>,
    apps: &[(String, String)],
    os_app_installs: &[String],
) -> Result<()> {
    bootstrap::seed_cedar_policies(state, policy_seed);
    state.server.rebuild_reaction_dispatcher();
    bootstrap::hydrate_entities(state, apps).await;
    bootstrap::recover_cedar_policies(state).await;
    bootstrap::recover_wasm_modules(state).await;
    bootstrap::recover_secrets(state).await;
    bootstrap::bootstrap_tenants(state, apps, bootstrap::EarlierOperatorKeys::from_env()).await;
    bootstrap::bootstrap_installed_apps(state, os_app_installs).await
}

/// Start the runtime the server serves with: the HttpEndpoint route tables
/// and their reconciler, and the state timers re-armed from their rows.
pub(super) async fn start_serving(state: &PlatformState, _later: Later) {
    // Subscribes to entity state changes; rebuilds the per-tenant route table
    // on every HttpEndpoint transition (ADR-0056 Phase 2 slice 3b).
    temper_server::http_endpoint::spawn_reconciler(state.server.clone());
    // Prime the route tables, so HttpEndpoint rows already in the store are
    // routable on the first request (the reconciler only reacts to changes).
    let tenants: Vec<temper_runtime::tenant::TenantId> = {
        let reg = state.server.registry.read().unwrap(); // ci-ok: infallible lock
        reg.tenant_ids().iter().map(|t| (*t).clone()).collect()
    };
    for tenant in tenants {
        temper_server::http_endpoint::rebuild_tenant_table(&state.server, &tenant).await;
    }
    // State timers re-armed from their rows (ADR-0056) before the first
    // request can act on their records, and runtime metrics.
    let _boot_timers = state.server.start_runtime().await;
}
