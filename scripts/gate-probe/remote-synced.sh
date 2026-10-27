# Probe sandbox: one gate_probe run on a SYNCED store, as production runs it: the database on the tlfs drive
# (/var/lib/temper/gate-probe-<tag>/probe.db) copied to a local tursodb 0.7.2 sync server (files on the VM disk),
# standing in for Turso Cloud. The fsync shim times every fsync. Usage: remote-synced.sh <tag> <probe mode args...>
tag=$1; shift
dir=/var/lib/temper/gate-probe-$tag; mkdir -p "$dir" /tmp/sync-server-$tag
cp /tmp/gate_probe2 "$HOME/gate_probe2" && chmod +x "$HOME/gate_probe2"; cp /tmp/fsync_spy.so "$HOME/fsync_spy.so"
( cd /tmp/sync-server-$tag && RUST_LOG=warn exec /tmp/turso_cli-x86_64-unknown-linux-gnu/tursodb /tmp/sync-server-$tag/remote.db --sync-server 127.0.0.1:8090 < /dev/null > /tmp/sync-server-$tag.log 2>&1 ) &
server=$!
for i in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:8090/ && break; sleep 0.2; done
echo "server pid $server up after ${i} tries; free $(free -m | awk '/Mem:/{print $7}') MB"
( echo 1000 > /proc/self/oom_score_adj; PROBE_SYNC_URL=http://127.0.0.1:8090 LD_PRELOAD="$HOME/fsync_spy.so" exec "$HOME/gate_probe2" "$dir/probe.db" "$@" ) 2> "/tmp/gate-synced-$tag.err"
echo "probe exit $?; free $(free -m | awk '/Mem:/{print $7}') MB"
kill $server 2>/dev/null; wait $server 2>/dev/null
grep -v "^SPY" "/tmp/gate-synced-$tag.err" | tail -3 | cut -c1-300
ls -la "$dir" | awk 'NR>3{print $5, $9}'
python3 - "/tmp/gate-synced-$tag.err" <<'PY'
import sys, collections
by = collections.defaultdict(list)
for line in open(sys.argv[1]):
    if line.startswith("SPY "):
        _, call, at, ms, *path = line.split()
        name = (path[0] if path else "?").rsplit("/", 1)[-1]
        by[(call, name)].append(float(ms))
for (call, name), ts in sorted(by.items()):
    ts.sort(); n = len(ts)
    print(f"fsync {name}: n {n} total {sum(ts):.0f} ms p50 {ts[n//2]:.1f} p90 {ts[int(n*.9)]:.1f} p99 {ts[int(n*.99)]:.1f} max {ts[-1]:.1f}")
PY
