//! The one call every embedder makes to start a server's background work, and
//! whether the server is still starting.

use std::sync::atomic::Ordering;

use temper_runtime::tenant::TenantId;

use super::{BootTimers, ServerState};

impl ServerState {
    /// Start this server's background runtime. Every embedder (`temper serve`,
    /// an embedding server) calls it once at boot, after its specs, Cedar policies and
    /// WASM modules are loaded and its apps installed, and before requests can
    /// act on records: a timer re-armed here may fire at once.
    ///
    /// It repairs catalog rows behind their journals for every registered tenant,
    /// then restores saved timers and reconciles repaired records through the boot
    /// fire gate before returning. It starts runtime metrics and the background
    /// carries that the installed versions hold for stored records
    /// (`spec_change::carry_pass`). Returns the background walk that recovers
    /// missing timers from the recorded state clock;
    /// it ends with the counts per tenant and type, which it also logs as
    /// `state timers after boot`. A second call in the same process starts
    /// nothing and returns None: a second re-arm would arm every row again.
    #[tracing::instrument(skip_all, fields(otel.name = "boot.start_runtime"))]
    pub async fn start_runtime(&self) -> Option<tokio::task::JoinHandle<BootTimers>> {
        if self.runtime_started.swap(true, Ordering::SeqCst) {
            tracing::warn!("server runtime already started in this process; not started again");
            return None;
        }
        self.spawn_runtime_metrics_loop();
        let boot = self.rearm_state_timers().await;
        // Report once the boot's timers are armed, even when none are, and
        // from now on make timers wait again by the wall clock after a resume.
        self.due_times.start();
        self.spawn_carry_stranded(self.tenants_with_carries());
        Some(boot)
    }

    /// Mark this server starting: its embedder answers before its boot ends,
    /// and holds every request but `/healthz` until [`Self::mark_serving`].
    /// A server never marked starting has served since its embedder first
    /// answered.
    pub fn mark_starting(&self) {
        self.starting.send_replace(true);
    }

    /// Mark this server serving: its runtime has started and its embedder's
    /// boot has ended, so requests act on its records from now on.
    pub fn mark_serving(&self) {
        self.starting.send_replace(false);
    }

    /// Whether this server is still starting (see [`Self::mark_starting`]).
    pub fn is_starting(&self) -> bool {
        *self.starting.borrow()
    }

    /// Wait until this server is no longer starting.
    pub async fn until_serving(&self) {
        let mut serving = self.starting.subscribe();
        // The sender lives in this server's state, so it outlives the wait.
        let _ = serving.wait_for(|starting| !*starting).await;
    }

    /// Start the runtime of `tenant`, restored after this server serves (see
    /// [`crate::tenant_restore`]): what [`Self::start_runtime`] did at boot
    /// for the tenants it served then. Call it once the tenant's specs, Cedar
    /// policies and WASM modules are loaded and its apps installed, while it
    /// is still starting, so no request acts on its records first. Err, with
    /// nothing armed, when its store could not be read: a later call starts
    /// over.
    #[tracing::instrument(skip_all, fields(otel.name = "restore.start_tenant_runtime", tenant = %tenant))]
    pub async fn start_tenant_runtime(&self, tenant: &TenantId) -> Result<(), String> {
        assert!(
            self.runtime_started.load(Ordering::SeqCst),
            "a tenant restored after serving starts after the server's runtime"
        );
        let _walk = self.rearm_tenant_timers(tenant).await?;
        if self.tenants_with_carries().contains(tenant) {
            self.spawn_carry_stranded(vec![tenant.clone()]);
        }
        Ok(())
    }
}
