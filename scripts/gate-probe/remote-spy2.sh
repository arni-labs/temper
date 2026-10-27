# STAGING canary sandbox: runs gate_probe (/tmp/gate_probe2) with the fsync-timing shim (/tmp/fsync_spy.so) on a scratch
# database on the tlfs drive or the VM disk, killed first if memory runs out. Prints the probe's JSON and a summary of
# every fsync it made, by file. Usage: remote-spy.sh <tlfs|vm> <tag> <probe mode args...>
place=$1; tag=$2; shift 2
if [ "$place" = tlfs ]; then dir=/var/lib/temper/gate-probe-$tag; else dir=/tmp/gate-probe-$tag; fi
mkdir -p "$dir"
cp /tmp/gate_probe2 "$HOME/gate_probe2" && chmod +x "$HOME/gate_probe2"; cp /tmp/fsync_spy.so "$HOME/fsync_spy.so"
app() { echo "free $(free -m | awk '/Mem:/{print $7}') MB"; }
app
( echo 1000 > /proc/self/oom_score_adj; LD_PRELOAD="$HOME/fsync_spy.so" exec "$HOME/gate_probe2" "$dir/probe.db" "$@" ) 2> "/tmp/gate-spy-$tag.err"
echo "probe exit $?"
app
grep -v "^SPY" "/tmp/gate-spy-$tag.err" | tail -3 | cut -c1-300
python3 - "/tmp/gate-spy-$tag.err" <<'PY'
import sys, collections
by = collections.defaultdict(list)
for line in open(sys.argv[1]):
    if line.startswith("SPY "):
        _, call, at, ms, *path = line.split()
        name = (path[0] if path else "?").rsplit("/", 1)[-1]
        by[(call, name)].append(float(ms))
for (call, name), ts in sorted(by.items()):
    ts.sort(); n = len(ts)
    print(f"{call} {name}: n {n} total {sum(ts):.0f} ms p50 {ts[n//2]:.1f} p90 {ts[int(n*.9)]:.1f} p99 {ts[int(n*.99)]:.1f} max {ts[-1]:.1f}")
PY
