//! Trims each tenant's WASM invocation log off the module call path.
//!
//! Recording an invocation only inserts and marks its tenant dirty. One
//! drain task, started by the first mark and ended when nothing is dirty,
//! trims dirty tenants one at a time. Marks that arrive while a tenant is
//! being trimmed coalesce into one more pass, so work follows inserts and no
//! timer runs when the log is quiet.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Pending {
    dirty: BTreeSet<String>,
    draining: bool,
}

/// Tenants whose invocation log may be above its bound.
#[derive(Clone, Default)]
pub(crate) struct WasmLogPruner {
    pending: Arc<Mutex<Pending>>,
}

impl WasmLogPruner {
    /// Mark `tenant` dirty. True when the caller must start the drain task.
    pub(crate) fn mark(&self, tenant: &str) -> bool {
        let mut pending = self.pending.lock().expect("pruner lock");
        pending.dirty.insert(tenant.to_string());
        !std::mem::replace(&mut pending.draining, true)
    }

    /// Take the next dirty tenant; when none is left the drain task is over.
    pub(crate) fn next(&self) -> Option<String> {
        let mut pending = self.pending.lock().expect("pruner lock");
        let next = pending.dirty.pop_first();
        if next.is_none() {
            pending.draining = false;
        }
        next
    }
}

impl crate::state::ServerState {
    /// Note that `tenant` logged an invocation and wake the pruner.
    pub(crate) fn schedule_wasm_log_prune(&self, tenant: &str) {
        if !self.wasm_log_pruner.mark(tenant) {
            return;
        }
        let state = self.clone();
        tokio::spawn(
            // determinism-ok: background trim of the WASM invocation log
            async move {
                while let Some(tenant) = state.wasm_log_pruner.next() {
                    let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
                        continue;
                    };
                    if let Err(e) = store.prune_wasm_invocations(&tenant).await {
                        tracing::error!(%tenant, error = %e, "failed to prune WASM invocation log");
                    }
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_drain_task_serves_every_dirty_tenant() {
        let pruner = WasmLogPruner::default();
        assert!(pruner.mark("b"), "the first mark starts the drain");
        assert!(!pruner.mark("a"), "a second mark joins it");
        assert!(!pruner.mark("a"), "a repeated mark coalesces");
        assert_eq!(pruner.next().as_deref(), Some("a"));
        assert_eq!(pruner.next().as_deref(), Some("b"));
        assert_eq!(pruner.next(), None);
        assert!(pruner.mark("a"), "after the drain ended, a mark restarts it");
    }
}
