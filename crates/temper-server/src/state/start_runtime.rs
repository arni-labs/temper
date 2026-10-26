//! The one call every embedder makes to start a server's background work.

use std::sync::atomic::Ordering;

use super::{BootTimers, ServerState};

impl ServerState {
    /// Start this server's background runtime. Every embedder (`temper serve`,
    /// temperpaw) calls it once at boot, after its specs, Cedar policies and
    /// WASM modules are loaded and its apps installed, and before requests can
    /// act on records: a timer re-armed here may fire at once.
    ///
    /// It arms again the state timers and scheduled actions the last process
    /// left waiting, from their rows (`rearm_state_timers`), and starts the runtime
    /// metrics loop. Returns the background walk that makes every record of a
    /// timed type follow its state, arming those in a timed state with no row
    /// from when they entered it; it ends with the counts per tenant, type and
    /// timed state, which it also logs as `state timers after boot`. A second
    /// call in the same process starts
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
