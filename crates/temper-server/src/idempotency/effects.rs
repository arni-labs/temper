//! One dispatcher at a time runs a keyed reply's post-dispatch effects.
//!
//! A retry can reach the actor while the first request's dispatcher is still
//! running the effects of the same reply (webhooks, integrations, spawns,
//! timers). The retry waits for that dispatcher instead of running them again.
//! When it finishes, the retry answers with the reply it completed. When it
//! ends without completing (an error or a dropped request), the next dispatcher
//! in line runs them, so a retry after a dropped reply still gets its effects.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tokio::sync::watch;

use super::{IdempotencyCache, RequestFingerprint};
use crate::entity_actor::EntityResponse;

/// The dispatchers running effects right now:
/// (actor_key, idempotency_key) → a receiver that closes when that one ends.
#[derive(Default)]
pub(super) struct EffectsOwners(Mutex<BTreeMap<(String, String), watch::Receiver<()>>>);

impl EffectsOwners {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), watch::Receiver<()>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether this dispatcher runs a reply's post-dispatch effects.
pub enum EffectsClaim<'a> {
    /// The effects have already run. Carries the reply the dispatcher that ran
    /// them completed, when this claim waited for it.
    Applied(Option<EntityResponse>),
    /// This dispatcher runs them, and completes the claim when they have run.
    Owner(EffectsOwner<'a>),
}

/// The right to run one reply's post-dispatch effects. Dropping it without
/// [`EffectsOwner::complete`] hands the effects to the next dispatcher.
pub struct EffectsOwner<'a> {
    cache: &'a IdempotencyCache,
    actor_key: String,
    idem_key: String,
    _finished: watch::Sender<()>,
}

impl EffectsOwner<'_> {
    /// Record that the effects ran, with the reply they produced. Retries of
    /// the same request are answered with it.
    pub fn complete(self, request: &RequestFingerprint, response: &EntityResponse) {
        self.cache
            .put_effects_applied(&self.actor_key, &self.idem_key, request, response);
    }
}

impl Drop for EffectsOwner<'_> {
    fn drop(&mut self) {
        // The sender drops after this, which wakes every waiter; by then the
        // key has no owner, so the first waiter to look takes it over.
        self.cache
            .effects_owners
            .lock()
            .remove(&(self.actor_key.clone(), self.idem_key.clone()));
    }
}

impl IdempotencyCache {
    /// Claim the post-dispatch effects of the successful reply for
    /// `(actor_key, idem_key)`. Waits while another dispatcher runs them.
    pub async fn claim_effects(&self, actor_key: &str, idem_key: &str) -> EffectsClaim<'_> {
        let owner_key = (actor_key.to_string(), idem_key.to_string());
        let mut waited = false;
        loop {
            let mut finished = {
                let mut owners = self.effects_owners.lock();
                if let Some(applied) = self.applied_reply(actor_key, idem_key) {
                    return EffectsClaim::Applied(waited.then_some(applied));
                }
                match owners.get(&owner_key) {
                    Some(finished) => finished.clone(),
                    None => {
                        let (sender, receiver) = watch::channel(());
                        owners.insert(owner_key, receiver);
                        return EffectsClaim::Owner(EffectsOwner {
                            cache: self,
                            actor_key: actor_key.to_string(),
                            idem_key: idem_key.to_string(),
                            _finished: sender,
                        });
                    }
                }
            };
            // Nothing is ever sent: this returns once the owner's sender drops.
            let _ = finished.changed().await;
            waited = true;
        }
    }
}
