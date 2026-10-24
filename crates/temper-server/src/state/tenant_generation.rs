//! Each life of a tenant id is a generation. A tenant deleted and created again under the same id is a new
//! generation, and work of the old one (an actor, a sleeping timer or follow-on, a queued projection write) is
//! refused when it acts, so it never lands in the new tenant or in storage that was removed.
//!
//! Work that writes to a tenant outside its actors holds a [`GenerationLease`] while it runs. Removing a tenant
//! closes its generation, so no new lease or actor is admitted, and waits for the leases already held before its
//! storage goes. Work that waits (a timer, a follow-on, a queued write) keeps only the [`TenantStamp`] of the
//! generation that made it and asks for a lease of that stamp when it acts: [`TenantGenerations::lease`] is the one
//! check. While a lease of generation N is held, the tenant's live generation is N or none, never a later one: the
//! next generation starts only after every lease of N has ended and N's storage is gone.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use temper_runtime::tenant::TenantId;
use tokio::sync::Notify;

/// One generation of a tenant id: the tenant and its incarnation number in this process.
///
/// Numbers come from one counter per process. Everything a stamp guards lives in memory, so nothing that carries a
/// stamp outlives the process that numbered it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantStamp {
    tenant: TenantId,
    generation: u64,
}

impl TenantStamp {
    /// The tenant this generation belongs to.
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The generation's number; a tenant created again gets a larger one.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The live generation of every tenant id this process has seen.
#[derive(Debug, Default)]
pub struct TenantGenerations {
    inner: Mutex<Generations>,
}

#[derive(Debug, Default)]
struct Generations {
    /// The number the last generation started with.
    last: u64,
    /// Each tenant's current generation: live, or closed while the tenant is removed.
    current: BTreeMap<TenantId, Arc<Generation>>,
}

#[derive(Debug)]
pub(crate) struct Generation {
    stamp: TenantStamp,
    work: Mutex<Work>,
    drained: Notify,
}

#[derive(Debug, Default)]
struct Work {
    closed: bool,
    leases: usize,
}

impl Generation {
    /// How many leases of this generation are held.
    pub(crate) fn leases(&self) -> usize {
        self.work().leases
    }

    fn work(&self) -> std::sync::MutexGuard<'_, Work> {
        self.work
            .lock()
            .expect("tenant generation work lock poisoned")
    }

    /// Admit one more piece of work, unless the generation is closed.
    fn admit(self: &Arc<Self>) -> Option<GenerationLease> {
        let mut work = self.work();
        if work.closed {
            return None;
        }
        work.leases += 1;
        Some(GenerationLease {
            generation: Arc::clone(self),
        })
    }
}

/// Work of one tenant generation that is running now. While any lease of a generation is held, the generation's
/// storage is kept and no later generation of the tenant starts.
#[derive(Debug)]
pub struct GenerationLease {
    generation: Arc<Generation>,
}

impl GenerationLease {
    /// The generation this work belongs to.
    pub fn stamp(&self) -> &TenantStamp {
        &self.generation.stamp
    }
}

impl Drop for GenerationLease {
    fn drop(&mut self) {
        let mut work = self.generation.work();
        debug_assert!(work.leases > 0, "a lease is counted while it is held");
        work.leases -= 1;
        if work.leases == 0 && work.closed {
            self.generation.drained.notify_waiters();
        }
    }
}

impl TenantGenerations {
    fn inner(&self) -> std::sync::MutexGuard<'_, Generations> {
        self.inner.lock().expect("tenant generations lock poisoned")
    }

    /// The tenant's current generation, starting one when its id has none. Closed while the tenant is removed.
    fn current(&self, tenant: &TenantId) -> Arc<Generation> {
        let mut inner = self.inner();
        if let Some(generation) = inner.current.get(tenant) {
            return Arc::clone(generation);
        }
        inner.last += 1;
        let generation = Arc::new(Generation {
            stamp: TenantStamp {
                tenant: tenant.clone(),
                generation: inner.last,
            },
            work: Mutex::new(Work::default()),
            drained: Notify::new(),
        });
        inner
            .current
            .insert(tenant.clone(), Arc::clone(&generation));
        generation
    }

    /// The tenant's current generation, live or being removed, starting one when its id has none.
    pub(crate) fn stamp(&self, tenant: &TenantId) -> TenantStamp {
        self.current(tenant).stamp.clone()
    }

    /// The tenant's live generation, starting one when its id has none, or None while the tenant is removed.
    pub fn live(&self, tenant: &TenantId) -> Option<TenantStamp> {
        let generation = self.current(tenant);
        let closed = generation.work().closed;
        (!closed).then(|| generation.stamp.clone())
    }

    /// Admit new work for the tenant's live generation, or None while the tenant is removed.
    pub fn lease_live(&self, tenant: &TenantId) -> Option<GenerationLease> {
        self.current(tenant).admit()
    }

    /// Admit work of the generation `stamp` names: None when that generation is closed or a later one has started.
    pub fn lease(&self, stamp: &TenantStamp) -> Option<GenerationLease> {
        let generation = self.current(&stamp.tenant);
        if generation.stamp != *stamp {
            return None;
        }
        generation.admit()
    }

    /// Close the tenant's current generation: no new lease, and no new actor, is admitted for it. Returns the closed
    /// generation, to wait on. Closing a closed generation again returns it as it is.
    pub(crate) fn close(&self, tenant: &TenantId) -> Arc<Generation> {
        let generation = self.current(tenant);
        generation.work().closed = true;
        generation
    }

    /// Wait until every lease of the closed `generation` has ended.
    pub(crate) async fn drained(generation: &Generation) {
        debug_assert!(generation.work().closed, "only a closed generation drains");
        loop {
            let notified = generation.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if generation.leases() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Forget the tenant's closed generation once its storage is gone, so the id's next use starts a new one. A live
    /// generation is kept.
    pub(crate) fn forget(&self, tenant: &TenantId) {
        let mut inner = self.inner();
        let closed = inner
            .current
            .get(tenant)
            .is_some_and(|generation| generation.work().closed);
        if closed {
            inner.current.remove(tenant);
        }
    }
}

/// The tenant generation that queued work belongs to, with what it needs to ask for a lease of it when it writes.
/// Queued work takes one when it is queued and writes only under [`GenerationRef::lease`], so a write of a deleted
/// generation is dropped instead of landing in the tenant created again under its id.
#[derive(Debug, Clone)]
pub struct GenerationRef {
    generations: Arc<TenantGenerations>,
    stamp: TenantStamp,
}

impl GenerationRef {
    /// A lease of the generation, while it is live.
    pub fn lease(&self) -> Option<GenerationLease> {
        self.generations.lease(&self.stamp)
    }
}

impl super::ServerState {
    /// The generation of `tenant` that work queued now belongs to: the current one, live or being removed.
    pub(crate) fn generation_ref(&self, tenant: &TenantId) -> GenerationRef {
        GenerationRef {
            generations: Arc::clone(&self.tenant_generations),
            stamp: self.tenant_generations.stamp(tenant),
        }
    }

    /// The one admission check for work of `tenant`. Work whose context carries a stamp of this tenant (a timer, a
    /// follow-on, a reaction or a callback of earlier work) is admitted only while that generation is live; other
    /// work joins the live generation. Returns the lease to hold while the work runs and the context, stamped with
    /// the generation, for the work it starts. None while the tenant is removed, or when the work's generation no
    /// longer exists. The context is boxed because dispatch futures hold it across their awaits, and they nest.
    pub fn admit_tenant_work(
        &self,
        tenant: &TenantId,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Option<(GenerationLease, Box<crate::request_context::AgentContext>)> {
        let lease = match agent_ctx
            .tenant_generation
            .as_ref()
            .filter(|stamp| stamp.tenant() == tenant)
        {
            Some(stamp) => self.tenant_generations.lease(stamp),
            None => self.tenant_generations.lease_live(tenant),
        }?;
        let mut stamped = Box::new(agent_ctx.clone());
        stamped.tenant_generation = Some(lease.stamp().clone());
        Some((lease, stamped))
    }

    /// A lease of `tenant`'s live generation for a write outside its actors (a create, a field update, a delete),
    /// or an error while the tenant is removed.
    pub(crate) fn lease_tenant(&self, tenant: &TenantId) -> Result<GenerationLease, String> {
        self.tenant_generations
            .lease_live(tenant)
            .ok_or_else(|| format!("tenant '{tenant}' is being deleted"))
    }

    /// `agent_ctx` stamped with the generation of `tenant` that work deferred from it belongs to: its own stamp of
    /// this tenant, or else the live generation. None while the tenant is removed.
    pub(crate) fn stamp_tenant_work(
        &self,
        tenant: &TenantId,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Option<crate::request_context::AgentContext> {
        if agent_ctx
            .tenant_generation
            .as_ref()
            .is_some_and(|stamp| stamp.tenant() == tenant)
        {
            return Some(agent_ctx.clone());
        }
        let mut stamped = agent_ctx.clone();
        stamped.tenant_generation = Some(self.tenant_generations.live(tenant)?);
        Some(stamped)
    }
}

#[cfg(test)]
#[path = "tenant_generation_test.rs"]
mod tests;
