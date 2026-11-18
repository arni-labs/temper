//! How many anonymous bundle exports run at once, and the queue for the rest.
//!
//! The anonymous bundle route takes no credential, and an export holds its
//! whole bundle in memory while it builds the answer, so how many run at once
//! is bounded by the memory this process has. A request past that waits its
//! turn here instead of being refused: every request gets one real answer, and
//! a burst (a TemperPaw boot asks for every installed app at once) costs time,
//! not memory.

use std::future::Future;

use tokio::sync::Semaphore;

use crate::genesis_install::BUNDLE_EXPORT_HELD_BYTES;

/// Run `export` once one of the process's anonymous export places is free.
pub(super) async fn admitted<T>(export: impl Future<Output = T>) -> T {
    in_turn(places(), export).await
}

/// Run `export` once one of `places` is free, holding it until `export` ends.
pub(super) async fn in_turn<T>(places: &Semaphore, export: impl Future<Output = T>) -> T {
    // `places` is never closed, so `acquire` only ever waits.
    let _place = places.acquire().await;
    export.await
}

/// The process's anonymous export places, sized the first time an export asks.
pub(super) fn places() -> &'static Semaphore {
    static PLACES: std::sync::OnceLock<Semaphore> = std::sync::OnceLock::new();
    PLACES.get_or_init(|| {
        let free = free_memory_bytes();
        let places = exports_that_fit(free);
        tracing::info!(
            free_memory_bytes = free,
            export_bytes = BUNDLE_EXPORT_HELD_BYTES,
            places,
            "anonymous Genesis bundle exports sized to free memory"
        );
        Semaphore::new(places)
    })
}

/// As many exports as `free` bytes hold at once, and never fewer than one: an
/// export that fits the per-bundle budget always runs, one at a time if that
/// is all there is room for. With no memory figure, one at a time.
pub(super) fn exports_that_fit(free: Option<u64>) -> usize {
    let fit = free.unwrap_or(0) / BUNDLE_EXPORT_HELD_BYTES;
    usize::try_from(fit).unwrap_or(usize::MAX).max(1)
}

/// The memory this process may still take, in bytes.
///
/// In a container, its cgroup's limit less what it already uses: Railway,
/// where Genesis runs, sets `memory.max` (24 GB on the staging registry) while
/// `/proc/meminfo` reports the whole host (384 GiB there), so the cgroup is
/// the only true figure. Outside a limited cgroup, the kernel's own estimate
/// of memory available to start new work. Elsewhere (macOS) there is no figure.
fn free_memory_bytes() -> Option<u64> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let limit = read("/sys/fs/cgroup/memory.max").and_then(|max| max.trim().parse::<u64>().ok());
    if let Some(limit) = limit {
        let used = read("/sys/fs/cgroup/memory.current")
            .and_then(|used| used.trim().parse::<u64>().ok())
            .unwrap_or(0);
        return Some(limit.saturating_sub(used));
    }
    let meminfo = read("/proc/meminfo")?;
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(kib.saturating_mul(1024))
}
