# Summarise gate_probe output lines (JSON plus fsync lines) read from stdin.
import sys, json
for l in sys.stdin:
    l = l.strip()
    if l.startswith('{'):
        d = json.loads(l)
        print(f"  ok {d['moves_ok']} failed {d['moves_failed']} rate {d['moves_per_s']:.1f}/s in {d['secs']:.1f}s")
        for k in ('append_hold_us', 'append_gate_wait_ms', 'append_total_us', 'low_priority_gate_wait_ms'):
            print(f"  {k} {d[k]}")
        print(f"  in flight first {d['in_flight_first_2s']} last {d['in_flight_last_2s']} max {d['in_flight_max']}")
        for k in ('latency_from_arrival_us', 'push_before_answer_us'):
            if k in d: print(f"  {k} {d[k]}")
        for u in d.get('upkeep', [])[:12]: print('  upkeep', u)
        if d.get('big_move'):
            print(f"  big {d['big_move']} behind-it latency ms {d.get('latency_ms_of_moves_arriving_5s_after_big')}")
        print(f"  slow holds {d['slow_holds_at_ms_hold_ms'][:20]}")
    elif l.startswith(('fsync', 'free', 'probe exit', 'thread', 'open store', 'server', 'move ')):
        print('  ' + l[:200])
