//! A deletion may remove credential storage before platform cleanup fails. Keep
//! only the authority to finish that deletion, never a reusable plaintext secret
//! or authority for another route or another generation. This state is process-local,
//! like the generation whose unfinished removal it accompanies.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;
use temper_authz::AuthenticatedRequestContext;
use temper_server::state::TenantStamp;

/// A one-way credential fingerprint passed only to the deletion handler.
#[derive(Clone)]
pub(crate) struct DeletionCredential([u8; 32]);

impl DeletionCredential {
    /// Discard the reusable secret after fingerprinting it.
    pub(crate) fn new(credential: String) -> Self {
        Self(Sha256::digest(credential.as_bytes()).into())
    }
}

/// Set only by middleware that matched a saved deletion credential and generation.
#[derive(Clone)]
pub(crate) struct AuthorizedDeletionRetry;

/// Authorization retained while a generation's deletion remains unfinished.
#[derive(Default)]
pub(crate) struct DeletionRetries {
    authorized: Mutex<BTreeMap<(TenantStamp, [u8; 32]), AuthenticatedRequestContext>>,
}

impl DeletionRetries {
    /// Match only the original credential and generation.
    pub(crate) fn resolve(
        &self,
        stamp: &TenantStamp,
        credential: &DeletionCredential,
    ) -> Option<AuthenticatedRequestContext> {
        let entries = self
            .authorized
            .lock()
            .expect("deletion retries lock poisoned");
        entries.get(&(stamp.clone(), credential.0)).cloned()
    }

    /// Called only after the delete handler has checked same-tenant and Cedar permission.
    pub(crate) fn remember(
        &self,
        stamp: &TenantStamp,
        credential: DeletionCredential,
        context: &AuthenticatedRequestContext,
    ) {
        self.authorized
            .lock()
            .expect("deletion retries lock poisoned")
            .entry((stamp.clone(), credential.0))
            .or_insert_with(|| context.clone());
    }

    /// End retry authority when removal finishes (including an already removed generation).
    pub(crate) fn forget(&self, stamp: &TenantStamp) {
        self.authorized
            .lock()
            .expect("deletion retries lock poisoned")
            .retain(|(generation, _), _| generation != stamp);
    }
}
