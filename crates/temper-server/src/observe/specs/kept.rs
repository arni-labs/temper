//! Passing checks, kept by the digest of what their levels read
//! ([`temper_verify::VerificationCascade::digest`]).
//!
//! A spec whose levels read what an earlier passing check's levels read gets
//! that verdict back instead of running them again, so a change to text no
//! level reads, such as an error message, is not checked again. A verdict is
//! kept in this process and in the platform blob table, so a restart keeps
//! it whatever checker reached it, as a joint proof's is
//! (`joint-verdict/<digest>`). A check that failed or ran out of its budget
//! is not kept: the next one asks again.

use std::sync::PoisonError;

use temper_verify::{CascadeResult, VerificationCascade};

use crate::state::ServerState;

fn blob_key(digest: &str) -> String {
    format!("check-verdict/{digest}")
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
    let in_memory = state
        .check_verdicts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(digest)
        .cloned();
    let result = match in_memory {
        Some(result) => result,
        None => {
            let store = state.platform_metadata_store()?;
            let bytes = match store.get_blob(&blob_key(digest)).await {
                Ok(bytes) => bytes?,
                Err(error) => {
                    tracing::warn!(%digest, %error, "a kept check could not be read");
                    return None;
                }
            };
            let result: CascadeResult = serde_json::from_slice(&bytes).ok()?;
            state
                .check_verdicts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(digest.to_string(), result.clone());
            result
        }
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
    state
        .check_verdicts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(digest.to_string(), result.clone());
    if let Some(store) = state.platform_metadata_store()
        && let Ok(bytes) = serde_json::to_vec(result)
        && let Err(error) = store.put_blob(&blob_key(digest), &bytes).await
    {
        tracing::warn!(%digest, %error, "a passing check could not be kept");
    }
}
