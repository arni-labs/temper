//! Scratch only: the deepest stack address seen at named points.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The stack address where the test body starts.
pub static TEST_START: AtomicUsize = AtomicUsize::new(0);
/// The deepest address seen on entry to the single projection upsert.
pub static UPSERT_ENTRY: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The deepest address seen just before a driver call in the upsert.
pub static BEFORE_DRIVER: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The address of a local in this frame.
#[inline(never)]
pub fn here() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Record this point's stack address in `slot` if it is the deepest yet.
pub fn mark(slot: &AtomicUsize) {
    slot.fetch_min(here(), Ordering::Relaxed);
}

/// Depths below the test start, in KiB.
pub fn report() -> String {
    let start = TEST_START.load(Ordering::Relaxed);
    let depth = |slot: &AtomicUsize| start.saturating_sub(slot.load(Ordering::Relaxed)) / 1024;
    format!(
        "upsert entry depth {} KiB, before-driver depth {} KiB",
        depth(&UPSERT_ENTRY),
        depth(&BEFORE_DRIVER)
    )
}
