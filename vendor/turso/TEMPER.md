# turso 0.7.2, vendored for Temper

The published crate, patched in two places (the workspace `Cargo.toml` points
`[patch.crates-io]` here):

1. `src/sync.rs`: `HTTP_WAIT` (8 s). `IoWorker::process_http` gives each sync
   request that long for its response head, then for each chunk of its body,
   and fails the request when it runs out. Upstream waits with no limit.
2. `src/lib.rs`: `#![allow(warnings, clippy::all)]`, because a path dependency's
   lints are not capped the way a registry crate's are.

Why: the sync engine's IO worker serves its queue one item at a time, and its
HTTP client sets no timeouts. A request the remote never answers (a kept
connection whose answer was lost) held every later push, and every other step
that waits on the worker, until the operating system gave the connection up.
Temper's server timed each push out at 10 s and, after 6 in a row, exited so
a restart opened a fresh connection. In production, fresh requests to the same
remote were answering in under a second while the engine's pushes kept
timing out, so apps restarted over a stall that a new connection already
cleared. With the limit, the stuck request ends, hyper drops its connection,
and the next request opens a new one; the server no longer exits for it.

8 s sits under Temper's 10 s push attempt and above the slowest answer a
fresh request to a remote got during those stalls (4.7 s). The limit is per
step, not per request, so a long restore that keeps sending body chunks is not
cut. It would cut a long-poll pull longer than 8 s; Temper never pulls (a
restore sets the file aside and bootstraps).

Left out, because they need a Turso admin server or a local sync server and
CI runs `cargo test` in every workspace (`scripts/ci/workspaces.py`):
`examples/`, `tests/`, the `#[cfg(test)]` modules at the end of `lib.rs`,
`sync.rs`, `params.rs` and `transaction.rs`, the dev-dependencies and their
manifest entries. Two upstream `TODO` comments are reworded, because the
pre-commit check covers vendored code too. Nothing else is changed. The
upstream crate is MIT licensed (https://github.com/tursodatabase/turso).

To move to a new turso, copy the published crate here and make the same
changes again, or drop this copy once upstream bounds its requests.
