//! Passing checks, kept by the digest of what their levels read
//! ([`temper_verify::VerificationCascade::digest`]).
//!
//! A spec whose levels read what an earlier passing check's levels read gets
//! that verdict back instead of running them again, so a change to text no
//! level reads, such as an error message, is not checked again. A verdict is
//! kept in the platform blob table under the kernel's own keys
//! ([`KERNEL_BLOB_PREFIX`]), which no caller's blob is written to, so a
//! restart keeps it whatever checker reached it, as a joint proof's is. A
//! server with no platform store keeps verdicts in this process. A check
//! that failed or ran out of its budget is not kept: the next one asks again.

use std::sync::PoisonError;

use temper_verify::{CascadeResult, VerificationCascade};

use crate::blob_store::KERNEL_BLOB_PREFIX;
use crate::state::ServerState;

fn blob_key(digest: &str) -> String {
    format!("{KERNEL_BLOB_PREFIX}check-verdict/{digest}")
}

/// The digest of what `cascade`'s levels read, worked out on a blocking
/// thread: it builds the models.
pub(super) async fn digest(
    cascade: impl FnOnce() -> VerificationCascade + Send + 'static,
) -> Option<String> {
    tokio::task::spawn_blocking(move || cascade().digest())
        .await
        .ok()
        .flatten()
}

/// The passing verdict kept under `digest`, with `kept` set, if any.
pub(super) async fn kept(state: &ServerState, digest: &str) -> Option<CascadeResult> {
    let result = match state.platform_metadata_store() {
        Some(store) => match store.get_blob(&blob_key(digest)).await {
            Ok(bytes) => serde_json::from_slice(&bytes?).ok()?,
            Err(error) => {
                tracing::warn!(%digest, %error, "a kept check could not be read");
                return None;
            }
        },
        None => state
            .check_verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(digest)
            .cloned()?,
    };
    Some(CascadeResult {
        kept: Some(digest.to_string()),
        ..result
    })
}

/// Keep `result` under `digest` when it passed every level it ran.
pub(super) async fn keep(state: &ServerState, digest: &str, result: &CascadeResult) {
    if !result.all_passed || result.stopped.is_some() {
        return;
    }
    let Some(store) = state.platform_metadata_store() else {
        state
            .check_verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(digest.to_string(), result.clone());
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(result)
        && let Err(error) = store.put_blob(&blob_key(digest), &bytes).await
    {
        tracing::warn!(%digest, %error, "a passing check could not be kept");
    }
}
