#!/usr/bin/env python3
"""List missing timer rows from a consistent SQLite backup, without arming them.

Run with Python 3.11+: audit-missing-state-timers.py /path/to/backup.db
Use the event/metadata database containing specs, snapshots, events and
state_timers. If tenants use separate databases, run on each backup. Take the
backup through the store's supported backup mechanism, including its WAL;
a bare copy of a live database is not a consistent inventory.
"""
import argparse
from datetime import datetime
import json
from pathlib import Path
import sqlite3
import tomllib


def is_tombstone(event_type, event):
    """Mirror PersistenceEnvelope::is_tombstone, including legacy journals."""
    if event_type != "Deleted":
        return False
    if "kind" in event:
        return event["kind"] == "tombstone"
    return (event.get("to_status") == "Deleted"
            and event.get("params") == {}
            and event.get("idempotency_key") is None)


def inventory(path):
    """Return missing-row records grouped by tenant and type, from durable facts."""
    with sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True) as db:
        db.execute("BEGIN")
        specs = {
            (tenant, kind): tomllib.loads(source)
            for tenant, kind, source in db.execute(
                "SELECT tenant, entity_type, ioa_source FROM specs"
            )
        }
        rows = set(db.execute("SELECT tenant, entity_type, entity_id FROM state_timers"))
        states = {}
        deleted = set()
        for tenant, kind, record, seq, snapshot in db.execute(
            "SELECT tenant, entity_type, entity_id, sequence_nr, snapshot FROM snapshots"
        ):
            restored = json.loads(snapshot)
            states[tenant, kind, record] = (seq, restored["status"], restored.get("status_clock", {}))
        # Do not rely on projections: a failed projection can also lose arming.
        for tenant, kind, record, seq, event_type, payload in db.execute(
            "SELECT tenant, entity_type, entity_id, sequence_nr, event_type, payload FROM events "
            "ORDER BY tenant, entity_type, entity_id, sequence_nr"
        ):
            key = (tenant, kind, record)
            event = json.loads(payload)
            if is_tombstone(event_type, event):
                deleted.add(key)
            if seq <= states.get(key, (-1, "", {}))[0]:
                continue
            status = event.get("to_status") or states.get(key, (-1, "", {}))[1]
            clock = states.get(key, (-1, "", {}))[2]
            if event.get("from_status") != event.get("to_status"):
                clock = {"entered_at": event.get("timestamp"), "actions": {}}
            else:
                clock.setdefault("actions", {})[event["action"]] = event.get("timestamp")
            states[key] = (seq, status, clock)
        grouped = {}
        for key, (_, status, clock) in sorted(states.items()):
            tenant, kind, record = key
            if key in rows or key in deleted:
                continue
            for timeout in specs.get((tenant, kind), {}).get("state_timeout", []):
                if timeout["state"] == status:
                    actions = clock.get("actions", {})
                    reset = timeout.get("reset_on", [])
                    starts = [clock.get("entered_at")] + [actions.get(action) for action in reset]
                    started = max((datetime.fromisoformat(t) for t in starts if t), default=None)
                    fired = actions.get(timeout["on_timeout"])
                    if (started and fired and datetime.fromisoformat(fired) >= started
                            and timeout["on_timeout"] not in reset):
                        continue  # This non-repeating timer already fired in its state.
                    action = next((a for a in specs[tenant, kind].get("action", [])
                                   if a["name"] == timeout["on_timeout"]), {})
                    needs_caller = any(isinstance(p, dict) and p.get("source") == "authenticated_subject"
                                       for p in action.get("params", []))
                    grouped.setdefault((tenant, kind), []).append({
                        "id": record, "state": status,
                        "action": timeout["on_timeout"],
                        "after_seconds": timeout["after_seconds"],
                        "clock_start": started.isoformat() if started else None,
                        "waits_for_caller": needs_caller,
                    })
        return [
            {"tenant": tenant, "type": kind, "count": len(records), "records": records}
            for (tenant, kind), records in sorted(grouped.items())
        ]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("backup", help="consistent SQLite backup (opened read-only)")
    print(json.dumps(inventory(parser.parse_args().backup), indent=2))
