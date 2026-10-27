# gate_probe (scratch)

Measures the store's write gate the way production writes: each move is one
`append_with_rows` (event, projection row, state timer) at High priority, then
a Low-priority trajectory, and on a synced store a push before the answer.
Source: `crates/temper-store-turso/examples/gate_probe/`.

## Build (Linux x86_64 from a Mac)

    rustup target add x86_64-unknown-linux-gnu
    CARGO_TARGET_DIR=$HOME/.cache/gate-probe-target cargo zigbuild --release \
      -p temper-store-turso --example gate_probe --target x86_64-unknown-linux-gnu.2.35
    zig cc -target x86_64-linux-gnu.2.35 -shared -fPIC -O2 -o fsync_spy.so fsync_spy.c -ldl

## Run

    gate_probe <db file> closed <moves>             # back to back: capacity
    gate_probe <db file> open <rate/s> <secs>       # Poisson arrivals: queue wait at a load
    gate_probe <db file> slow <rate/s> <secs> <kb>  # one big move at secs/2 among the others

Env: `PROBE_PAYLOAD_BYTES` (default 400), `PROBE_SYNC_URL` (synced mode; unset = local
file), `PROBE_SYNC_TOKEN` (default "local"), `PROBE_CHECKPOINT_MB` (default 16, the
upkeep's WAL size before a checkpoint).

Prints one JSON object: moves/s, append time total and under the permit (append
minus gate wait), gate waits by priority, in-flight counts, the big move and the
moves that arrived in the 5 s after it, push-before-answer time, and the upkeep
(push, trim, checkpoint) when synced. Latency is from the planned arrival, so a
move that waited for the runtime still counts its wait.

`LD_PRELOAD=fsync_spy.so` prints `SPY <call> <ms at end> <duration ms> <path>` for
every fsync and fdatasync on stderr; `remote-synced.sh` summarises them by file.

## Synced mode without Turso Cloud

`remote-synced.sh <tag> <mode args>` starts a local `tursodb` 0.7.2
`--sync-server 127.0.0.1:8090` (turso_cli release tarball) as the remote, runs the
probe with the database on the tlfs drive, and prints fsync totals per file.
`remote-spy2.sh <tlfs|vm> <tag> <mode args>` is the same for a local (unsynced) file
on the drive or on the VM disk.
