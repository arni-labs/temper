//! Process-wide bounds for production blob I/O.
//!
//! The semaphore is an admission slot for object-store operations (puts,
//! heads, and opening a get). A `$value` download releases the slot once
//! response headers arrive. Holding it for the body would queue other blob
//! work behind idle network time — the 20s R2 stalls this slot exists to
//! keep from spreading. The body keeps a separate stream permit and fails if
//! a chunk does not arrive within [`BLOB_READ_IDLE_TIMEOUT`].

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Semaphore;

const DEFAULT_BLOB_IO_MAX_CONCURRENCY: usize = 32;

pub(super) const BLOB_IO_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const BLOB_BUFFERED_OPERATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a single body read may sit idle before the stream fails.
///
/// This is a per-chunk budget, not a cap on the whole transfer: a download
/// that keeps receiving bytes can run longer than 30s.
pub(super) const BLOB_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn blob_io_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| {
        let limit = std::env::var("TEMPER_BLOB_IO_MAX_CONCURRENCY") // determinism-ok: startup-only tuning knob
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_BLOB_IO_MAX_CONCURRENCY);
        Arc::new(Semaphore::new(limit))
    }))
}
