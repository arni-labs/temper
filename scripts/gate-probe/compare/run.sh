#!/bin/bash
# One gate_probe run on a synced store, as production keeps it: the database on the tlfs drive, copied to a local
# tursodb 0.8.3 sync server (VM disk) standing in for Turso Cloud. Same instrument as scripts/gate-probe/remote-synced.sh.
# Usage: run.sh <wal|mvcc> <tag> <probe mode args...>   (a probe still draining after 300 s is stopped: no result)
journal=$1; tag=$2; shift 2
home=/home/tl-user/m
dir=/var/lib/temper/m-$tag
mkdir -p "$dir" "$home/results" /tmp/sync-$tag
# A store prepared by prep-mvcc.sh keeps the remote and port it was bound to.
port=$(cat /tmp/sync-$tag.port 2>/dev/null || echo $((18500 + RANDOM % 400)))
( cd /tmp/sync-$tag && RUST_LOG=warn exec "$home/tursodb" /tmp/sync-$tag/remote.db --sync-server 127.0.0.1:$port < /dev/null > /tmp/sync-$tag.log 2>&1 ) &
server=$!
for i in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:$port/ && break; sleep 0.2; done
mvcc=""; [ "$journal" = mvcc ] && mvcc=1
echo "== $tag ($journal) $* | server up after $i tries; free $(free -m | awk '/Mem:/{print $7}') MB"
( echo 1000 > /proc/self/oom_score_adj
  TEMPER_TURSO_MVCC=$mvcc PROBE_SYNC_URL=http://127.0.0.1:$port LD_PRELOAD="$home/fsync_spy.so" \
    exec timeout 300 "${PROBE_BIN:-$home/gate_probe}" "$dir/probe.db" "$@" ) 2> "/tmp/m-$tag.err" | tee "$home/results/$tag.json"
echo "probe exit ${PIPESTATUS[0]}; free $(free -m | awk '/Mem:/{print $7}') MB"
kill $server 2>/dev/null; wait $server 2>/dev/null
grep -v "^SPY" "/tmp/m-$tag.err" | grep -v "liveness coverage" | tail -5 | cut -c1-300
echo "header bytes 18-19 (2 = WAL, 255 = MVCC):$(od -An -tu1 -j18 -N2 "$dir/probe.db")"
ls -la "$dir" | awk 'NR>1{print $5, $9}'
python3 - "/tmp/m-$tag.err" <<'PY'
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
