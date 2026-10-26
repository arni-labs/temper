"""Regression checks for the pre-roll missing timer inventory."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "audit", Path(__file__).with_name("audit-missing-state-timers.py"))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class InventoryDeletionTests(unittest.TestCase):
    def test_live_deleted_state_and_actual_tombstones(self):
        for snapshot_seq in (None, 2):
            with self.subTest(snapshot_seq=snapshot_seq), tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "backup.db"
                with sqlite3.connect(path) as db:
                    db.executescript("""
                        CREATE TABLE specs (tenant, entity_type, ioa_source);
                        CREATE TABLE state_timers (tenant, entity_type, entity_id);
                        CREATE TABLE snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot);
                        CREATE TABLE events (tenant, entity_type, entity_id, sequence_nr, event_type, payload);
                    """)
                    db.execute("INSERT INTO specs VALUES (?, ?, ?)", ("t", "Record", '''
[[state_timeout]]
state = "Deleted"
on_timeout = "Purge"
after_seconds = 60
'''))
                    base = {"from_status": "Active", "to_status": "Deleted",
                            "action": "Deleted", "params": {},
                            "timestamp": "2026-10-26T10:00:00Z"}
                    cases = {
                        "live_transition": ("Deleted", {**base, "kind": "transition"}),
                        "live_other_action": ("Retire", base),
                        "live_legacy_params": ("Deleted", {**base, "params": {"x": 1}}),
                        "live_legacy_key": ("Deleted", {**base, "idempotency_key": "key"}),
                        "deleted_explicit": ("Deleted", {**base, "kind": "tombstone"}),
                        "deleted_legacy": ("Deleted", base),
                    }
                    for record, (event_type, payload) in cases.items():
                        db.execute("INSERT INTO events VALUES (?, ?, ?, ?, ?, ?)",
                                   ("t", "Record", record, 1, event_type, json.dumps(payload)))
                        if snapshot_seq is not None:
                            snapshot = {"status": "Deleted", "status_clock": {
                                "entered_at": base["timestamp"], "actions": {}}}
                            db.execute("INSERT INTO snapshots VALUES (?, ?, ?, ?, ?)",
                                       ("t", "Record", record, snapshot_seq, json.dumps(snapshot)))
                result = audit.inventory(path)
                self.assertEqual(len(result), 1)
                self.assertEqual(result[0]["count"], 4)
                self.assertEqual({r["id"] for r in result[0]["records"]},
                                 {key for key in cases if key.startswith("live_")})
                self.assertTrue(all(r["action"] == "Purge" for r in result[0]["records"]))


class InventoryCompositeTests(unittest.TestCase):
    def test_composite_after_snapshot_preserves_state_clock(self):
        for snapshot in (False, True):
            with self.subTest(snapshot=snapshot), tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "backup.db"
                with sqlite3.connect(path) as db:
                    db.executescript("""
                        CREATE TABLE specs (tenant, entity_type, ioa_source);
                        CREATE TABLE state_timers (tenant, entity_type, entity_id);
                        CREATE TABLE snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot);
                        CREATE TABLE events (tenant, entity_type, entity_id, sequence_nr, event_type, payload);
                    """)
                    db.execute("INSERT INTO specs VALUES (?, ?, ?)", ("t", "Record", '''
[[state_timeout]]
state = "Waiting"
on_timeout = "Expire"
after_seconds = 60
'''))
                    entered = "2026-10-26T10:00:00Z"
                    if snapshot:
                        db.execute("INSERT INTO snapshots VALUES (?, ?, ?, ?, ?)",
                                   ("t", "Record", "r", 1, json.dumps({"status": "Waiting", "status_clock": {"entered_at": entered, "actions": {}}})))
                    else:
                        db.execute("INSERT INTO events VALUES (?, ?, ?, ?, ?, ?)",
                                   ("t", "Record", "r", 1, "Start", json.dumps({"action": "Start", "from_status": "Idle", "to_status": "Waiting", "timestamp": entered})))
                    db.execute("INSERT INTO events VALUES (?, ?, ?, ?, ?, ?)",
                               ("t", "Record", "r", 2, "CompositeEvent", json.dumps({"parent_action": "Run", "timestamp": "2026-10-26T10:01:00Z"})))
                records = audit.inventory(path)[0]["records"]
                self.assertEqual(len(records), 1)
                self.assertEqual(records[0]["state"], "Waiting")
                self.assertEqual(records[0]["clock_start"], "2026-10-26T10:00:00+00:00")


if __name__ == "__main__":
    unittest.main()
