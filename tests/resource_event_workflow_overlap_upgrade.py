"""Preserve an actual F120 event schedule while enabling Workflow overlap protection."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

from support import Failure, key, target
from test_resource_event_workflow_overlap import ResourceEventWorkflowOverlap
from test_schedule import timestamp


class ResourceEventWorkflowOverlapUpgrade(ResourceEventWorkflowOverlap):
    def setUp(self):
        self.current = Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).resolve() / "psiosd"
        with patch.dict(os.environ, {"PSIOS_TEST_BIN_DIR": str(self.previous)}):
            super().setUp()

    def verify(self):
        return json.loads(subprocess.check_output([*self.n.daemon, "verify"], timeout=20))

    def create_refused(self, request, overlap):
        with self.assertRaises(Failure) as denied:
            self.n.call("Schedule.Create", {**request, "event": dict(request["event"], overlap=overlap)})
        self.assertEqual(denied.exception.code, "InvalidArgument", denied.exception)

    def test_old_event_schedule_keeps_its_task_only_overlap_after_migration(self):
        n = self.n
        request = {"key": key(), "tool": "workflow.start",
            "input": {"definition_ref": self.definition, "max_runtime_seconds": 60},
            "delegation_id": self.scope["delegation_ref"]["resource_id"],
            "event": {"source_ref": self.source, "condition": {"kind": "SpecChanged"},
                      "expires_at": timestamp(90), "max_triggers": 2}}
        self.create_refused(request, "SkipWhileWorkflowActive")
        schedule = n.call("Schedule.Create", request)
        schedule_id = schedule["reference"]["resource_id"]
        self.assertNotIn("overlap", n.get(schedule_id)["resource"]["spec"]["event"])
        self.stop()
        first_source = self.change_source(63)
        first_state, (first,) = self.wait_triggers(schedule_id, 1)
        launch_one, first_workflow, first_child = self.launch(first)
        self.assertEqual(self.budget(), (1, 1))

        self.stop()
        old_daemon = n.daemon[:]
        source = self.verify()
        self.assertEqual(source["database_format"], 120, source)
        self.assertFalse(source["migration_required"], source)
        n.daemon[0] = str(self.current)
        staged = self.verify()
        self.assertEqual((staged["database_format"], staged["node_id"], staged["migration_required"]),
                         (120, source["node_id"], True), staged)
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), first_state)
        self.assertEqual(self.workflow(first_workflow["reference"]["resource_id"]), first_workflow)
        self.assertEqual(n.wait(first_child["task_id"]), first_child)

        self.stop()
        self.source = self.change_source(62)
        completed, triggers = self.wait_triggers(schedule_id, 2)
        self.assertEqual(triggers[0], first)
        self.assertEqual(completed["state"]["lifecycle"], "Completed")
        self.assertNotIn("overlap", completed["state"])
        launch_two, second_workflow, second_child = self.launch(triggers[1])
        self.assertNotEqual(launch_one, launch_two)
        self.assertEqual(self.workflow(first_workflow["reference"]["resource_id"]), first_workflow)
        self.assert_source_events(triggers, [first_source, self.source])
        self.assertEqual(self.budget(), (2, 2))
        for workflow, child in ((first_workflow, first_child), (second_workflow, second_child)):
            cancelled = n.call("Workflow.Cancel", target(workflow["reference"]))
            self.assertEqual(cancelled["state"]["lifecycle"], "Cancelled")
            self.assertEqual(n.wait(child["task_id"])["lifecycle"], "Cancelled")
        self.assertEqual(self.budget(), (0, 2))
        self.assertFalse(self.file.exists())

        malformed = {**request, "key": key(), "event": dict(request["event"], source_ref=self.source)}
        for option in (None, "UnsupportedOverlap"):
            self.create_refused(malformed, option)
        protected, new_id = self.create(count=2, max_runtime=50)
        self.stop()
        self.change_source(61)
        _, (new_first,) = self.wait_triggers(new_id, 1)
        launch, waiting, _ = self.launch(new_first)
        self.stop()
        self.change_source(60)
        current, (_, skipped) = self.wait_triggers(new_id, 2)
        self.assert_skip(skipped, new_first, launch, waiting)
        self.assertEqual(self.budget(), (1, 3))
        self.assertEqual(n.call("Schedule.Create", request), completed)
        self.stop()
        report = self.verify()
        self.assertGreaterEqual(report["database_format"], 121, report)
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["node_id"], source["node_id"])
        old = subprocess.run([*old_daemon, "serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(old.returncode, 0, old)
        self.assertIn("SchemaVersionUnsupported", old.stderr)
        n.start()
        self.assertEqual(n.call("Schedule.Create", protected), current)
        self.assertFalse(self.file.exists())


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True, type=Path)
    ResourceEventWorkflowOverlapUpgrade.previous = parser.parse_args().previous_bin_dir.resolve()
    unittest.main(argv=[__file__, "ResourceEventWorkflowOverlapUpgrade."
                        "test_old_event_schedule_keeps_its_task_only_overlap_after_migration"])
