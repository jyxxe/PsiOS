"""Preserve an actual F118 timed backlog and fence the old writer after F119."""

import argparse
from datetime import datetime, timedelta
import json
import os
from pathlib import Path
import signal
import subprocess
import unittest
from unittest.mock import patch

from support import Failure, key
from test_schedule import after, timestamp
from test_schedule_coalescence import ScheduleCoalescence


class ScheduleCoalescenceUpgrade(ScheduleCoalescence):
    def setUp(self):
        self.current = Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).resolve() / "psiosd"
        with patch.dict(os.environ, {"PSIOS_TEST_BIN_DIR": str(self.previous)}):
            super().setUp()

    def verify(self):
        return json.loads(subprocess.check_output([*self.n.daemon, "verify"], timeout=20))

    def test_old_timed_backlog_is_not_reinterpreted_as_coalescence(self):
        node = self.n
        self.set_scheduling("Cordoned")
        start = timestamp(1)
        end = (datetime.fromisoformat(start) + timedelta(seconds=30)).isoformat(timespec="milliseconds")
        request = {"key": key(), "tool": "system.status", "input": {}, "run_at": start,
                   "deadline_at": end, "delegation_id": self.grant["delegation_ref"]["resource_id"],
                   "repeat": {"interval_seconds": 2, "count": 4, "overlap": "Allow",
                              "missed": {"mode": "BoundedCatchUp", "limit": 2}}}
        with self.assertRaises(Failure) as denied:
            node.call("Schedule.Create", {**request, "repeat": dict(request["repeat"],
                       missed={"mode": "Coalesce"})})
        self.assertEqual(denied.exception.code, "InvalidArgument", denied.exception)
        initial = node.call("Schedule.Create", request)
        self.assertEqual(self.budget(), (0, 0))
        node.stop(signal.SIGKILL)
        old_daemon = node.daemon[:]
        source = self.verify()
        self.assertEqual(source["database_format"], 118, source)
        self.assertFalse(source["migration_required"], source)
        node.daemon[0] = str(self.current)
        staged = self.verify()
        self.assertEqual((staged["database_format"], staged["node_id"], staged["migration_required"]),
                         (118, source["node_id"], True), staged)
        node.start()
        self.assertEqual(self.state(initial), initial)
        after(start, 6.1)
        self.assertEqual(self.state(initial), initial)
        self.set_scheduling("Enabled")
        completed = self.complete(initial)
        first, second, third, fourth = self.triggers(completed)
        self.assertEqual(completed["state"]["trigger_refs"][0]["resource_id"],
                         initial["state"]["next"]["trigger_id"])
        for trigger in (first, second):
            self.assertEqual((trigger["lifecycle"], trigger["reason"]), ("Skipped", "CatchUpLimit"), trigger)
            self.assertIsNone(trigger["task_receipt"])
        older_tasks = set()
        for trigger in (third, fourth):
            self.assertEqual(trigger["lifecycle"], "Triggered", trigger)
            task_id = trigger["task_receipt"]["task_id"]
            older_tasks.add(task_id)
            self.assertEqual(node.wait(task_id)["lifecycle"], "Succeeded")
        for trigger in (first, second, third, fourth):
            self.assertNotIn("coalescence", trigger)
            self.assertNotIn("coalesced_into_trigger_id", trigger)
        self.assertEqual(len(older_tasks), 2)
        self.assertEqual(self.ids("Task"), older_tasks)
        self.assertEqual(self.budget(), (0, 2))

        self.set_scheduling("Cordoned")
        coalescing, pending = self.new_schedule(interval=2, window=20, count=2)
        after(coalescing["run_at"], 2.15)
        self.set_scheduling("Enabled")
        combined = self.complete(pending)
        task, _ = self.check_group(pending, combined, coalescing, expired=0)
        self.assertEqual(self.ids("Task"), older_tasks | {task["task_id"]})
        self.assertEqual(self.budget(), (0, 3))
        node.stop(signal.SIGKILL)
        report = self.verify()
        self.assertGreaterEqual(report["database_format"], 119, report)
        self.assertEqual(report["node_id"], source["node_id"])
        self.assertFalse(report["migration_required"], report)
        old = subprocess.run([*old_daemon, "serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(old.returncode, 0, old)
        self.assertIn("SchemaVersionUnsupported", old.stderr)
        node.start()
        self.assertEqual(node.call("Schedule.Create", request), completed)
        self.assertEqual(node.call("Schedule.Create", coalescing), combined)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True, type=Path)
    ScheduleCoalescenceUpgrade.previous = parser.parse_args().previous_bin_dir.resolve()
    unittest.main(argv=[__file__, "ScheduleCoalescenceUpgrade."
                        "test_old_timed_backlog_is_not_reinterpreted_as_coalescence"])
