//! The one call every embedder makes to start a server's background work.

use std::sync::atomic::Ordering;

use super::{BootTimers, ServerState};

impl ServerState {
    /// Start this server's background runtime. Every embedder (`temper serve`,
    /// temperpaw) calls it once at boot, after its specs, Cedar policies and
    /// WASM modules are loaded and its apps installed, and before requests can
    /// act on records: a timer re-armed here may fire at once.
    ///
    /// It starts recovery of the state timers and scheduled actions the last
    /// process left waiting (`rearm_state_timers`), and the runtime metrics loop.
    /// Returns immediately with the background recovery, including retries for
    /// unreadable timer rows. Each tenant's records follow their state after
    /// its saved callers have been restored, including records with no row;
    /// it ends with the counts per tenant and type, which it also logs as
    /// `state timers after boot`. A second call in the same process starts
    /// nothing and returns None: a second re-arm would arm every row again.
    pub async fn start_runtime(&self) -> Option<tokio::task::JoinHandle<BootTimers>> {
        if self.runtime_started.swap(true, Ordering::SeqCst) {
            tracing::warn!("server runtime already started in this process; not started again");
            return None;
        }
        self.spawn_runtime_metrics_loop();
        Some(self.rearm_state_timers().await)
    }
}
