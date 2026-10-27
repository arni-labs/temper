"""One line per probe run: summary.py <tag> <result json> <stderr with SPY lines>."""
import json
import sys

tag, result_path, err_path = sys.argv[1:4]
try:
    r = json.load(open(result_path))
except Exception as error:  # a run that died prints why in /tmp/run-<tag>.log
    print(f"{tag}: no result ({error})")
    sys.exit(0)
fsyncs = 0
fsync_ms = 0.0
for line in open(err_path):
    if line.startswith("SPY "):
        parts = line.split()
        if len(parts) > 4 and "probe.db" in parts[4]:
            fsyncs += 1
            fsync_ms += float(parts[3])
lat, hold, push = r["latency_from_arrival_us"], r["append_hold_us"], r["push_before_answer_us"]
ok = r["moves_ok"]
print(
    f"{tag}: ok {ok} failed {r['moves_failed']} | {r['moves_per_s']:.1f} moves/s | "
    f"latency p50 {lat['p50'] / 1000:.0f} p99 {lat['p99'] / 1000:.0f} max {lat['max'] / 1000:.0f} ms | "
    f"write hold p50 {hold['p50'] / 1000:.0f} p99 {hold['p99'] / 1000:.0f} ms | push p50 {push['p50'] / 1000:.0f} ms | "
    f"gate wait p99 {r['append_gate_wait_ms']['p99']} ms | in flight max {r['in_flight_max']} | "
    f"store fsyncs {fsyncs} ({fsyncs / max(ok, 1):.1f}/move, {fsync_ms:.0f} ms)"
)
