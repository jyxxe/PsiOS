"""Exercise a live legacy Skip series through the F117 workflow-overlap cutover."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import unittest
from unittest.mock import patch

from support import Failure, key, target
from test_schedule import timestamp
from test_workflow_schedule_overlap import WorkflowScheduleOverlap


class WorkflowScheduleOverlapUpgrade(WorkflowScheduleOverlap):
    def setUp(self):
        self.current_bin_dir = Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).absolute()
        with patch.dict(os.environ, {"PSIOS_TEST_BIN_DIR": self.previous_bin_dir}):
            super().setUp()

    def request(self, overlap, *, interval):
        return {"key": key(), "tool": "workflow.start", "run_at": timestamp(1), "deadline_at": timestamp(50),
            "input": {"definition_ref": self.definition, "max_runtime_seconds": 40},
            "delegation_id": self.scope["delegation_ref"]["resource_id"],
            "repeat": {"interval_seconds": interval, "count": 2, "missed": {"mode": "Skip"}, "overlap": overlap}}

    def scheduling(self, mode):
        node = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {"scheduling": mode, "expected_resource_version": node["version"]})

    def verify(self, command):
        return json.loads(subprocess.check_output(command + ["verify"], timeout=20))

    def cancel(self, run, child):
        done = self.n.call("Workflow.Cancel", target(run["reference"]))
        self.assertEqual(done["state"]["lifecycle"], "Cancelled", done)
        self.assertEqual(self.n.wait(child["task_id"])["lifecycle"], "Cancelled")

    def test_legacy_skip_keeps_its_semantics_after_migration(self):
        n = self.n
        with self.assertRaises(Failure) as rejected:
            n.call("Schedule.Create", self.request("SkipWhileWorkflowActive", interval=3))
        self.assertEqual(rejected.exception.code, "InvalidArgument", rejected.exception)
        old_request = self.request("Skip", interval=6)
        old_schedule = n.call("Schedule.Create", old_request)["reference"]["resource_id"]
        midway, (earliest,) = self.wait_triggers(old_schedule, 1)
        launch_one, first_run, first_child = self.launch(earliest, old_schedule)
        first_id = first_run["reference"]["resource_id"]
        self.assertEqual(self.budget(), (1, 1))
        self.assertFalse(self.path.exists())
        # Prevent the next window from racing the compatibility probes on a loaded runner.
        self.scheduling("Cordoned")
        self.assertEqual(self.read_schedule(old_schedule), midway)
        old_tasks = self.initial_tasks | {launch_one, first_child["task_id"]}
        self.assertEqual(self.ids("Task"), old_tasks)

        old_daemon = n.daemon[:]
        n.stop(signal.SIGKILL)
        source = self.verify(old_daemon)
        self.assertEqual(source["database_format"], 116, source)
        n.daemon[0] = str(self.current_bin_dir / "psiosd")
        staged = self.verify(n.daemon)
        self.assertTrue(staged["migration_required"], staged)
        self.assertEqual(staged["node_id"], source["node_id"])
        n.start()
        self.assertEqual(n.call("Schedule.Create", old_request), midway)
        self.assertEqual(self.read_workflow(first_id), first_run)
        self.assertEqual(n.wait(first_child["task_id"]), first_child)
        self.assertEqual(self.ids("Task"), old_tasks)
        self.assertEqual(self.budget(), (1, 1))

        self.scheduling("Enabled")
        completed, (retained, second) = self.wait_triggers(old_schedule, 2)
        self.assertEqual(retained, earliest)
        self.assertEqual(completed["state"]["lifecycle"], "Completed")
        self.assertEqual(completed["state"]["repeat"]["overlap"], "Skip")
        self.assertNotIn("overlap_evidence", second)
        launch_two, second_run, second_child = self.launch(second, old_schedule)
        second_id = second_run["reference"]["resource_id"]
        self.assertNotEqual(first_id, second_id)
        self.assertEqual(self.read_workflow(first_id), first_run)
        self.assertEqual(n.wait(first_child["task_id"]), first_child)
        self.assertEqual(self.ids("WorkflowRun"), {first_id, second_id})
        self.assertEqual(self.ids("Task"), old_tasks | {launch_two, second_child["task_id"]})
        self.assertEqual(self.budget(), (2, 2))
        self.cancel(first_run, first_child)
        self.assertEqual(self.read_workflow(second_id), second_run)
        self.cancel(second_run, second_child)
        self.assertFalse(self.path.exists())
        self.assertEqual(n.get(self.volume["resource_id"])["reference"], self.volume)
        self.assertEqual(self.budget(), (0, 2))

        new_request = self.request("SkipWhileWorkflowActive", interval=3)
        new_schedule = n.call("Schedule.Create", new_request)["reference"]["resource_id"]
        _, (first,) = self.wait_triggers(new_schedule, 1)
        launch_three, guarded_run, guarded_child = self.launch(first, new_schedule)
        guarded, (same_first, skipped) = self.wait_triggers(new_schedule, 2)
        self.assertEqual(same_first, first)
        proof = self.assert_skip(guarded, skipped, launch_three, guarded_run, "WaitingApproval")
        self.assertEqual(proof["reference"], guarded_run["reference"])
        self.assertEqual(n.call("Schedule.Create", old_request), completed)
        self.assertEqual(n.call("Schedule.Create", new_request), guarded)
        self.assertEqual(self.budget(), (1, 3))
        self.cancel(guarded_run, guarded_child)
        self.assertEqual(self.budget(), (0, 3))
        self.assertFalse(self.path.exists())

        n.stop()
        current = self.verify(n.daemon)
        self.assertGreaterEqual(current["database_format"], 117, current)
        self.assertEqual(current["node_id"], source["node_id"])
        self.assertFalse(current["migration_required"], current)
        old_writer = subprocess.run(old_daemon + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(old_writer.returncode, 0, old_writer)
        self.assertIn("SchemaVersionUnsupported", old_writer.stderr)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True)
    WorkflowScheduleOverlapUpgrade.previous_bin_dir = parser.parse_args().previous_bin_dir
    unittest.main(argv=[__file__, "WorkflowScheduleOverlapUpgrade.test_legacy_skip_keeps_its_semantics_after_migration"])
