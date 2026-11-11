//! What `temper serve` does between opening its store and serving.
//!
//! A server with many tenants serves before it restores them all.
//! [`read_stored`] reads the specs the store holds and registers the server's
//! own tenants' ([`Later::own_tenants`]), [`restore_own`] brings the rest of
//! their state back, and [`start_serving`] starts the runtime just before the
//! listener answers, and the walk that restores every other tenant after it
//! (`super::restore`). What a boot reads before it serves therefore does not
//! grow with the tenants it holds, except the one read per store of every
//! tenant's spec rows and recorded installs.

use std::collections::BTreeMap;

use std::future::IntoFuture;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use temper_platform::state::PlatformState;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::storage::StorageStack;

use super::bootstrap;
use super::installed_apps;
use super::later::Later;
use super::restore;

/// What a boot read from its store before it serves.
pub(super) struct Stored {
    /// The registry with the server's own tenants restored.
    pub registry: SpecRegistry,
    /// Cedar policy text of the `--app` tenants loaded from disk.
    pub policy_seed: BTreeMap<String, String>,
    /// The tenants restored after the server serves.
    pub later: Later,
}

/// Read the store's specs into a registry, and the `--app` specs from disk:
/// the server's own tenants registered, every other tenant's rows left for
/// after it serves.
pub(super) async fn read_stored(
    pg_pool: Option<&sqlx::PgPool>,
    storage_stack: Option<&StorageStack>,
    apps: &[(String, String)],
) -> Result<Stored> {
    let bootstrap_tenant = std::env::var("TEMPER_BOOTSTRAP_TENANT") // determinism-ok: read once at startup
        .ok()
        .filter(|tenant| !tenant.trim().is_empty());
    let mut later = Later::new(Later::own_tenants(apps, bootstrap_tenant.as_deref()));
    let (registry, policy_seed) =
        bootstrap::build_registry(pg_pool, storage_stack, apps, &mut later).await?;
    Ok(Stored {
        registry,
        policy_seed,
        later,
    })
}

/// Bring the server's own tenants back: the `--app` tenants' Cedar from disk
/// (`policy_seed`), their records, Cedar, modules, secrets, the system and
/// agent specs, and their installed apps. What any other tenant needs is left
/// in `later`.
pub(super) async fn restore_own(
    state: &PlatformState,
    policy_seed: BTreeMap<String, String>,
    apps: &[(String, String)],
    os_app_installs: &[String],
    later: &mut Later,
) -> Result<()> {
    bootstrap::seed_cedar_policies(state, policy_seed);
    state.server.rebuild_reaction_dispatcher();
    bootstrap::hydrate_entities(state, apps, later).await;
    bootstrap::recover_cedar_policies(state, later).await;
    bootstrap::recover_wasm_modules(state, later).await;
    bootstrap::recover_secrets(state, &mut later.secrets).await;
    let keys = bootstrap::EarlierOperatorKeys::from_env();
    bootstrap::bootstrap_tenants(state, apps, keys, later).await;
    installed_apps::bootstrap_installed_apps(state, os_app_installs, later).await
}

/// Start the runtime the server serves with: the HttpEndpoint route tables
/// and their reconciler, and the state timers re-armed from their rows; then
/// the walk that restores the tenants `later` holds, which are starting
/// until each is restored.
pub(super) async fn start_serving(state: &PlatformState, later: Later) {
    // Subscribes to entity state changes; rebuilds the per-tenant route table
    // on every HttpEndpoint transition (ADR-0056 Phase 2 slice 3b).
    temper_server::http_endpoint::spawn_reconciler(state.server.clone());
    // Prime the route tables, so HttpEndpoint rows already in the store are
    // routable on the first request (the reconciler only reacts to changes).
    let tenants: Vec<temper_runtime::tenant::TenantId> = {
        let reg = state
            .server
            .registry
            .read()
            .expect("registry lock poisoned");
        reg.tenant_ids().iter().map(|t| (*t).clone()).collect()
    };
    for tenant in tenants {
        temper_server::http_endpoint::rebuild_tenant_table(&state.server, &tenant).await;
    }
    // State timers re-armed from their rows (ADR-0056) before the first
    // request can act on their records, and runtime metrics.
    let _boot_timers = state.server.start_runtime().await;
    // Last, with nothing awaited after it: no other tenant is read before the
    // server serves.
    restore::spawn_walk(state.clone(), later);
    state.server.mark_serving();
}

/// Serve `router` on `listener` in the background from now on. The server is
/// starting until [`start_serving`] ends: `/healthz` answers at once and says
/// so, naming the boot step it is in, and every other request waits until
/// then, with no deadline, as it waited for the listener before. A boot step
/// still running is reported meanwhile ([`report_while_starting`]).
pub(super) fn serve(
    state: &PlatformState,
    listener: TcpListener,
    router: Router,
) -> JoinHandle<std::io::Result<()>> {
    state.server.mark_starting();
    // determinism-ok: the boot's report runs beside the boot, in this process only.
    tokio::spawn(report_while_starting(state.server.clone()));
    let router = router.layer(middleware::from_fn_with_state(
        state.server.clone(),
        wait_for_boot,
    ));
    // determinism-ok: the listener's task, as `axum::serve` was in `temper serve`.
    tokio::spawn(axum::serve(listener, router).into_future())
}

/// Hold every request but `/healthz` until the server serves.
async fn wait_for_boot(
    State(server): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() != "/healthz" {
        server.until_serving().await;
    }
    next.run(request).await
}

/// How often a boot still starting says which step it is in. No boot step has
/// a limit outside this code (a provider's, or the store's) to report at, so
/// each is reported once a minute, as a record's message still running is
/// (temper-server's `entity_actor::watchdog`). It reports; it never ends the
/// boot.
const REPORT_EVERY: Duration = Duration::from_secs(60);

/// Log `boot step still running` with the step, its tenant and how long it
/// has run, each [`REPORT_EVERY`] until the server serves. A log line, not a
/// span: a span is exported only when it closes, and a step that never ends
/// never closes one.
async fn report_while_starting(server: ServerState) {
    let mut every =
        tokio::time::interval_at(tokio::time::Instant::now() + REPORT_EVERY, REPORT_EVERY);
    loop {
        tokio::select! {
            () = server.until_serving() => return,
            _ = every.tick() => {}
        }
        match server.boot_steps.current() {
            Some(step) => tracing::warn!(
                step = %step.step,
                tenant = step.tenant.as_deref().unwrap_or(""),
                since_secs = step.since_secs,
                "boot step still running"
            ),
            None => tracing::warn!("boot step still running"),
        }
    }
}
