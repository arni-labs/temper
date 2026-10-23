//! Recursive dispatch on a stack that grows onto the heap.
//!
//! A boxed future keeps its state on the heap, but polling a future that
//! awaits another boxed future still nests one poll frame per level on the
//! caller's thread stack. Work that recurses as deep as its data (a reaction
//! chain, an inline integration callback that runs another action) polls each
//! level through [`on_growable_stack`], so a deep chain gets more stack instead
//! of overflowing the thread and taking the process down.

use futures_util::FutureExt;
use futures_util::future::BoxFuture;

// Both sizes are the ones the inline callback boundary has used since
// fa932974, where a self-callback test overflowed a small test thread's stack.
// They refuse nothing: they say when to switch segments and how big a new one
// is, and a segment is only allocated when a chain is that deep.

/// Stack that must be left before a level is entered; with less, the level
/// runs on a new segment.
const RED_ZONE: usize = 2 * 1024 * 1024;
/// Size of each new segment.
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// Build and poll the future `make` returns on a stack that grows when the
/// current one runs low, both while building it and on every poll.
pub(crate) fn on_growable_stack<'a, T: 'a>(
    make: impl FnOnce() -> BoxFuture<'a, T>,
) -> BoxFuture<'a, T> {
    stacker::maybe_grow(RED_ZONE, STACK_BYTES, || {
        let mut future = make();
        futures_util::future::poll_fn(move |context| {
            stacker::maybe_grow(RED_ZONE, STACK_BYTES, || future.as_mut().poll(context))
        })
        .boxed()
    })
}
