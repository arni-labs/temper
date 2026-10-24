//! Removing a tenant from the running server, in an order that keeps its storage until nothing can still write to
//! it: close the tenant's generation so no new work or actor is admitted, stop its actors and wait for them and for
//! the work still running to end, remove its storage, then forget what this process holds for it. Work of the closed
//! generation that acts later (a timer, a follow-on, a queued projection write) is refused, so the id created again
//! starts a new generation that nothing of the old one reaches. A removal acts on the generation it was admitted for,
//! so one still under way when another finished does not remove the tenant created again under the id.

use std::future::Future;

use futures_util::FutureExt;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;

use super::ServerState;
use super::tenant_generation::{Generation, TenantGenerations, TenantStamp};

/// Why a tenant removal did not finish. The tenant's generation stays closed (no new actors and no new work), its
/// specs, credentials and policies are kept, and running the removal again picks up where this one stopped.
#[derive(Debug)]
pub enum TenantRemovalError {
    /// A lock over the tenant's in-memory state was poisoned by a panic elsewhere.
    LockPoisoned(&'static str),
    /// Some of the tenant's actors, or work admitted for it, were still running when the wait ended.
    ActorsStillRunning {
        /// How many actors had not ended.
        running: usize,
        /// How many pieces of admitted work (leases of the tenant's generation) were still held.
        work: usize,
        /// How long the removal waited for them.
        waited: std::time::Duration,
    },
    /// Removing the tenant's storage failed.
    Storage(PersistenceError),
}

impl std::fmt::Display for TenantRemovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LockPoisoned(what) => write!(f, "the {what} lock is poisoned"),
            Self::ActorsStillRunning {
                running,
                work,
                waited,
            } => write!(
                f,
                "{running} of the tenant's actors and {work} pieces of its work were still running after {waited:?}; \
                 its storage was kept, retry the delete"
            ),
            Self::Storage(error) => write!(f, "removing the tenant's storage failed: {error}"),
        }
    }
}

impl std::error::Error for TenantRemovalError {}

fn poisoned<T>(what: &'static str) -> impl FnOnce(T) -> TenantRemovalError {
    move |_| TenantRemovalError::LockPoisoned(what)
}

impl ServerState {
    /// Remove the tenant generation `stamp` names: refuse its new work, stop its actors and wait until each has ended,
    /// then run `remove_storage`, then forget everything this process holds for the tenant. Returns what
    /// `remove_storage` returned, whether the tenant existed, or false when that generation was already removed.
    ///
    /// Storage is removed only once no actor of the tenant is running, so none can write into it afterwards (or into
    /// a database created again under the same id). Each step converges when repeated, so a removal that failed is
    /// finished by running it again.
    pub async fn remove_tenant<F, Fut>(
        &self,
        stamp: &TenantStamp,
        remove_storage: F,
    ) -> Result<bool, TenantRemovalError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<bool, PersistenceError>>,
    {
        let tenant = stamp.tenant();
        // A closed generation admits no new work and no new actor. Its specs, credentials and policies stay until its
        // storage is gone, so a removal that stops part way can be retried by the caller who started it.
        let Some(closing) = self.tenant_generations.close(stamp) else {
            return Ok(false);
        };
        // Storage and memory are keyed by the tenant id, which a tenant created again reuses. They are removed only
        // while `closing` is still the tenant's generation, by one removal at a time: one that waited behind a removal
        // that finished finds the generation gone and leaves the id to its new tenant.
        let _removing = closing.removing().await;
        if !self.tenant_generations.is_current(&closing) {
            return Ok(false);
        }
        self.stop_tenant_actors(tenant, &closing).await?;
        let existed = remove_storage()
            .await
            .map_err(TenantRemovalError::Storage)?;
        self.forget_tenant(tenant, &closing).await?;
        Ok(existed)
    }

    /// Halt every actor of `tenant` and wait until each has ended and the work admitted for its closed `generation`
    /// has finished, for as long as a dispatch waits for its actor (`action_dispatch_timeout`): a message still being
    /// handled after that has a caller that already gave up. A halt needs no mailbox space and drops the actor's
    /// queued messages, so a dispatch waiting on a halted actor ends. Actors that ended leave the registry; the rest
    /// stay there, halted, for the next attempt to wait on.
    async fn stop_tenant_actors(
        &self,
        tenant: &TenantId,
        generation: &Generation,
    ) -> Result<(), TenantRemovalError> {
        let prefix = format!("{tenant}:");
        let actors: Vec<_> = self
            .actor_registry
            .read()
            .map_err(poisoned("actor registry"))?
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(_, actor)| actor.clone())
            .collect();
        for actor in &actors {
            actor.halt();
        }
        let waited = self.action_dispatch_timeout;
        let all_ended = async {
            futures_util::future::join_all(actors.iter().map(|actor| actor.stopped())).await;
            TenantGenerations::drained(generation).await;
        };
        let finished = tokio::time::timeout(waited, all_ended).await.is_ok();
        let work = generation.leases();

        let mut registry = self
            .actor_registry
            .write()
            .map_err(poisoned("actor registry"))?;
        registry.retain(|key, actor| {
            !(key.starts_with(&prefix) && actor.stopped().now_or_never().is_some())
        });
        let running = registry
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .count();
        drop(registry);
        if finished && running == 0 && work == 0 {
            Ok(())
        } else {
            Err(TenantRemovalError::ActorsStillRunning {
                running,
                work,
                waited,
            })
        }
    }

    /// Drop everything this process holds in memory for a deleted tenant: its entity caches and indexes, its cached
    /// replies, its capture losses and markers, its timer sequences, its pending eventual invariants and queued snapshots,
    /// its HTTP routes, its Cedar policies, its cached secrets, its WASM modules and
    /// its specs. A tenant created again with the same id then starts from its own (empty) database, not from what
    /// the deleted tenant left in memory. [`Self::remove_tenant`] runs this once the tenant's actors have ended and
    /// its storage is gone, while `generation` is still the tenant's.
    async fn forget_tenant(
        &self,
        tenant: &TenantId,
        generation: &std::sync::Arc<Generation>,
    ) -> Result<(), TenantRemovalError> {
        let prefix = format!("{tenant}:");
        let owned = |key: &String| key.starts_with(&prefix);

        self.last_accessed
            .write()
            .map_err(poisoned("last accessed"))?
            .retain(|key, _| !owned(key));
        {
            let mut cache = self
                .entity_state_cache
                .lock()
                .map_err(poisoned("entity state cache"))?;
            let keys: Vec<String> = cache
                .iter()
                .map(|(key, _)| key)
                .filter(|key| owned(key))
                .cloned()
                .collect();
            for key in keys {
                cache.pop(&key);
            }
        }
        self.entity_index
            .write()
            .map_err(poisoned("entity index"))?
            .retain(|key, _| !owned(key));
        self.entity_index_hydrated
            .write()
            .map_err(poisoned("entity index hydration"))?
            .retain(|key| !owned(key));
        self.key_index_backfilled
            .write()
            .map_err(poisoned("key index backfill"))?
            .retain(|key, _| !owned(key));
        self.key_index_watermarks_loaded
            .write()
            .map_err(poisoned("key index watermarks"))?
            .remove(tenant.as_str());
        self.entity_observe_log
            .lock()
            .map_err(poisoned("entity observe log"))?
            .retain(|key, _| !owned(key));
        self.entity_event_sequences
            .lock()
            .map_err(poisoned("entity event sequences"))?
            .retain(|key, _| !owned(key));

        self.idempotency_cache.forget_tenant(tenant);
        crate::trajectory_outbox::forget_tenant_capture_loss_markers(tenant.as_str());
        self.state_timeout_tracker.forget_tenant(tenant);
        self.eventual_tracker
            .write()
            .map_err(poisoned("eventual invariants"))?
            .forget_tenant(tenant.as_str());
        if let Some(queue) = self
            .snapshot_write_queue
            .lock()
            .map_err(poisoned("snapshot write queue"))?
            .as_ref()
        {
            queue.forget_tenant(tenant);
        }
        self.http_endpoint_tables.remove(tenant).await;

        self.authz.remove_tenant(tenant.as_str());
        self.tenant_policies
            .write()
            .map_err(poisoned("tenant policies"))?
            .remove(tenant.as_str());
        if let Some(vault) = self.secrets_vault.as_ref() {
            vault.remove_tenant(tenant.as_str());
        }
        self.wasm_module_registry
            .write()
            .map_err(poisoned("WASM module registry"))?
            .remove_tenant(tenant);
        self.registry
            .write()
            .map_err(poisoned("spec registry"))?
            .remove_tenant(tenant);
        // Last: the id's next use starts a new generation only once nothing of this one is left in memory.
        self.tenant_generations.forget(generation);
        // The one place a generation's capture losses are retired: its deletion finished, so the sessions they would
        // mark went with its storage. After `forget`, so a loss counted from here on sees the generation removed and
        // is not counted (`CaptureHealth::record_unconfirmed_loss`), and one counted before is retired here.
        self.capture_health.forget_generation(generation.stamp());
        crate::runtime_metrics::record_server_state_metrics(self);
        Ok(())
    }
}

#[cfg(test)]
#[path = "tenant_removal_test.rs"]
mod tests;
