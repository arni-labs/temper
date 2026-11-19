//! How long a write request has to answer.
//!
//! A server whose store has a remote copy answers a write only after the
//! change reaches that copy (temper-cli's `serve::sync_copy`). The request's
//! run and that push share one budget, so a batch that starts its rows inside
//! the same budget leaves its push the time it needs.

use std::time::Duration;

/// How long a request that can change data may take, its push included,
/// before its change is reported as not confirmed.
pub const WRITE_ANSWER_BUDGET: Duration = Duration::from_secs(10);

/// The least time left for the push of a slow request. A batch starts no
/// row once less than this is left of [`WRITE_ANSWER_BUDGET`].
pub const MIN_PUSH_BUDGET: Duration = Duration::from_secs(1);
