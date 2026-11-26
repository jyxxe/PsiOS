"""A timed Workflow series must retain custody of its active child across restart."""

from contextlib import closing, contextmanager
from datetime import datetime
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Node, key, target
from test_schedule import timestamp


class WorkflowScheduleOverlap(unittest.TestCase):
    payload = b"Written by the scheduled Workflow.\n"

    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "workflow-overlap"})
        self.path = self.n.state / "volumes" / self.volume["resource_id"] / "data" / "proof"
        self.definition = self.n.call("Workflow.Define", {"key": key(), "steps": [{
            "id": "record", "tool": "volume.write", "after": [],
            "input": {**target(self.volume), "name": self.path.name, "content": self.payload.decode()},
        }]})["reference"]
        self.scope = self.n.call("Grant.Issue", {"principal_id": "local-owner", "units": 8, "ttl_seconds": 120,
            "actions": ["schedule.manage", "workflow.manage", "workflow.start", "task.submit", "task.read",
                        "volume.write"],
            "tool_targets": [self.definition["resource_id"], self.volume["resource_id"]]})
        self.initial_tasks = self.ids("Task")

    def ids(self, kind):
        return {ref["resource_id"] for ref in self.n.call("Resource.List", {"kind": kind})["items"]}

    def schedule(self, *, interval, max_runtime):
        request = {"key": key(), "tool": "workflow.start", "run_at": timestamp(1), "deadline_at": timestamp(30),
            "input": {"definition_ref": self.definition, "max_runtime_seconds": max_runtime},
            "delegation_id": self.scope["delegation_ref"]["resource_id"],
            "repeat": {"interval_seconds": interval, "count": 3, "missed": {"mode": "Skip"},
                       "overlap": "SkipWhileWorkflowActive"}}
        created = self.n.call("Schedule.Create", request)
        return request, created["reference"]["resource_id"]

    def until(self, read, ready):
        deadline = time.monotonic() + 12
        while True:
            result = read()
            if ready(result):
                return result
            self.assertLess(time.monotonic(), deadline, result)
            time.sleep(0.04)

    def read_schedule(self, schedule_id):
        return self.n.call("Schedule.Get", {"id": schedule_id})

    def wait_triggers(self, schedule_id, count):
        schedule = self.until(lambda: self.read_schedule(schedule_id),
                              lambda report: len(report["state"]["trigger_refs"]) >= count)
        triggers = []
        for ref in schedule["state"]["trigger_refs"]:
            trigger = self.n.call("Schedule.TriggerGet", {"id": ref["resource_id"]})
            self.assertEqual(trigger["reference"], ref)
            triggers.append(trigger["state"])
        return schedule, triggers

    def read_workflow(self, run_id):
        return self.n.call("Workflow.Get", {"id": run_id})

    def wait_run(self, run_id, phase):
        return self.until(lambda: self.read_workflow(run_id), lambda run: run["state"]["lifecycle"] == phase)

    def step(self, run):
        child, = run["state"]["steps"]
        self.assertEqual(child["step_id"], "record")
        return child

    def launch(self, trigger, schedule_id):
        self.assertEqual(trigger["lifecycle"], "Triggered", trigger)
        launch_id = trigger["task_receipt"]["task_id"]
        launched = self.n.wait(launch_id)
        self.assertEqual((launched["lifecycle"], launched["outcome_knowledge"]), ("Succeeded", "Known"), launched)
        self.assertEqual((launched["origin_ref"]["origin_type"], launched["origin_ref"]["origin_id"]),
                         ("Schedule", schedule_id))
        output, = launched["result"]["artifact_refs"]
        receipt = self.n.call("Artifact.Read", {"reference": output})
        self.assertEqual(receipt["definition_ref"], self.definition)
        self.assertEqual(receipt["origin_task_ref"]["resource_id"], launch_id)
        run_id = receipt["workflow_run_ref"]["resource_id"]
        waiting = self.wait_run(run_id, "WaitingApproval")
        self.assertEqual(waiting["state"]["origin_task_ref"], receipt["origin_task_ref"])
        child_id = self.step(waiting)["task_receipt"]["task_id"]
        child = self.n.wait(child_id)
        self.assertEqual((child["lifecycle"], child["outcome_knowledge"]), ("WaitingApproval", "NotStarted"), child)
        self.assertEqual((child["origin_ref"]["origin_type"], child["origin_ref"]["origin_id"]), ("WorkflowRun", run_id))
        return launch_id, waiting, child

    def budget(self):
        budget = self.n.call("Budget.Get", {"id": self.scope["budget_ref"]["resource_id"]})
        return budget["reserved"], budget["spent"]

    def assert_skip(self, schedule, trigger, launch_id, workflow, lifecycle):
        self.assertEqual((trigger["lifecycle"], trigger["reason"]), ("Skipped", "PreviousWorkflowActive"), trigger)
        self.assertIsNone(trigger["task_receipt"], trigger)
        evidence = trigger["overlap_evidence"]
        launch = self.n.wait(launch_id)
        artifact, = launch["result"]["artifact_refs"]
        self.assertEqual(evidence, {"launch_trigger_ref": schedule["state"]["trigger_refs"][0],
            "launch_task_ref": self.n.get(launch_id)["reference"], "launch_artifact_ref": artifact,
            "workflow_run_ref": evidence["workflow_run_ref"]}, evidence)
        self.assertEqual(evidence["workflow_run_ref"]["resource_id"], workflow["reference"]["resource_id"])
        exact = self.n.call("Resource.Resolve", {"reference": evidence["workflow_run_ref"]})
        self.assertEqual(exact["reference"], evidence["workflow_run_ref"])
        self.assertEqual(exact["value"]["lifecycle"], lifecycle, exact)
        self.assertEqual(exact["value"]["origin_task_ref"], workflow["state"]["origin_task_ref"])
        return exact

    def verify_stopped(self):
        report = json.loads(subprocess.check_output(self.n.daemon + ["verify"], timeout=20))
        self.assertEqual(report["sqlite_integrity"], "Verified", report)
        self.assertFalse(report["migration_required"], report)

    @contextmanager
    def withhold_receipt(self, task_id):
        database = self.n.state / "state.db"
        with closing(sqlite3.connect(database, timeout=10)) as db, db:
            db.execute(f"""CREATE TRIGGER withhold_workflow_schedule_receipt BEFORE INSERT ON artifact_chunks
                WHEN EXISTS(SELECT 1 FROM versions WHERE id=NEW.artifact_id AND spec->>'task_id'='{task_id}')
                BEGIN SELECT RAISE(ABORT, 'fixture withheld the known file write receipt'); END""")
        try:
            yield
        finally:
            with closing(sqlite3.connect(database, timeout=10)) as db, db:
                db.execute("DROP TRIGGER withhold_workflow_schedule_receipt")

    def test_waiting_workflow_skips_across_restart_and_next_occurrence_runs_after_cancel(self):
        request, schedule_id = self.schedule(interval=5, max_runtime=25)
        _, (earliest,) = self.wait_triggers(schedule_id, 1)
        launch_id, waiting, child = self.launch(earliest, schedule_id)
        child_id = child["task_id"]
        first_run = waiting["reference"]["resource_id"]
        first_tasks = self.initial_tasks | {launch_id, child_id}
        self.assertEqual(self.ids("Task"), first_tasks)
        self.assertEqual(self.ids("WorkflowRun"), {first_run})
        self.assertEqual(self.budget(), (1, 1))
        self.assertFalse(self.path.exists())

        self.n.stop(signal.SIGKILL)
        self.verify_stopped()
        self.n.start()
        current, (first, second) = self.wait_triggers(schedule_id, 2)
        self.assertEqual(first, earliest)
        evidence = self.assert_skip(current, second, launch_id, waiting, "WaitingApproval")
        self.assertEqual(evidence["reference"], waiting["reference"])
        self.assertEqual(current["state"]["last_task_id"], launch_id)
        self.assertEqual(self.read_workflow(first_run), waiting)
        self.assertEqual(self.n.wait(child_id), child)
        self.assertEqual(self.ids("Task"), first_tasks)
        self.assertEqual(self.ids("WorkflowRun"), {first_run})
        self.assertEqual(self.budget(), (1, 1))

        cancelled = self.n.call("Workflow.Cancel", target(waiting["reference"]))
        self.assertEqual(cancelled["state"]["lifecycle"], "Cancelled", cancelled)
        self.assertEqual(self.n.wait(child_id)["lifecycle"], "Cancelled")
        self.assertEqual(self.n.get(self.volume["resource_id"])["reference"], self.volume)
        self.assertEqual(self.budget(), (0, 1))
        self.assertFalse(self.path.exists())

        complete, (replayed_first, replayed_second, third) = self.wait_triggers(schedule_id, 3)
        self.assertEqual((replayed_first, replayed_second), (first, second))
        self.assertEqual(complete["state"]["lifecycle"], "Completed")
        new_launch, new_waiting, new_child = self.launch(third, schedule_id)
        new_run = new_waiting["reference"]["resource_id"]
        new_child_id = new_child["task_id"]
        self.assertNotEqual(new_run, first_run)
        self.assertEqual(self.ids("Task"), first_tasks | {new_launch, new_child_id})
        self.assertEqual(self.ids("WorkflowRun"), {first_run, new_run})
        self.assertEqual(self.budget(), (1, 2))
        self.assertFalse(self.path.exists())
        self.n.approve(new_child_id)
        final = self.wait_run(new_run, "Succeeded")
        self.assertEqual(self.step(final)["task_receipt"]["task_id"], new_child_id)
        self.assertEqual(self.path.read_bytes(), self.payload)
        file = self.path.stat()
        self.assertEqual(self.budget(), (0, 3))
        self.n.stop(signal.SIGKILL)
        self.verify_stopped()
        self.n.start()
        self.assertEqual(self.n.call("Schedule.Create", request), complete)
        self.assertEqual(self.read_workflow(new_run), final)
        self.assertEqual(self.ids("Task"), first_tasks | {new_launch, new_child_id})
        self.assertEqual((self.path.stat().st_ino, self.path.stat().st_mtime_ns), (file.st_ino, file.st_mtime_ns))

    def test_unknown_physical_write_skips_future_occurrences_even_after_workflow_deadline(self):
        request, schedule_id = self.schedule(interval=3, max_runtime=4)
        _, (first,) = self.wait_triggers(schedule_id, 1)
        launch_id, waiting, child = self.launch(first, schedule_id)
        first_run = waiting["reference"]["resource_id"]
        child_id = child["task_id"]
        with self.withhold_receipt(child_id):
            self.n.approve(child_id)
            unknown = self.n.wait(child_id)
            self.assertEqual((unknown["lifecycle"], unknown["outcome_knowledge"]), ("Paused", "Unknown"), unknown)
            blocked = self.wait_run(first_run, "Blocked")
            self.assertEqual((blocked["state"]["reason"], blocked["state"]["outcome_knowledge"]),
                             ("OutcomeUnknown", "Unknown"), blocked)
            self.assertEqual(self.path.read_bytes(), self.payload)
            original = self.path.stat()
            self.assertEqual(self.budget(), (0, 2))
            self.n.stop(signal.SIGKILL)
        self.verify_stopped()
        self.n.start()
        complete, (original_first, second, third) = self.wait_triggers(schedule_id, 3)
        self.assertEqual(original_first, first)
        for skipped in (second, third):
            exact = self.assert_skip(complete, skipped, launch_id, blocked, "Blocked")
            self.assertEqual(exact["value"]["outcome_knowledge"], "Unknown")
        self.assertGreater(datetime.fromisoformat(third["run_at"]), datetime.fromisoformat(waiting["deadline_at"]))
        self.assertEqual(complete["state"]["lifecycle"], "Completed")
        self.assertEqual(complete["state"]["last_task_id"], launch_id)
        after_deadline = self.until(lambda: self.read_workflow(first_run),
                                    lambda run: run["state"]["stop_reason"] == "DeadlineExceeded")
        self.assertEqual((after_deadline["state"]["lifecycle"], after_deadline["state"]["outcome_knowledge"],
                          after_deadline["state"]["stop_reason"]), ("Blocked", "Unknown", "DeadlineExceeded"),
                         after_deadline)
        self.assertEqual(self.step(after_deadline)["task_receipt"]["task_id"], child_id)
        self.assertEqual(self.ids("WorkflowRun"), {first_run})
        self.assertEqual(self.ids("Task"), self.initial_tasks | {launch_id, child_id})
        self.assertEqual(self.budget(), (0, 2))
        self.assertEqual(self.n.wait(child_id), unknown)
        self.assertEqual(self.path.read_bytes(), self.payload)
        self.assertEqual((self.path.stat().st_ino, self.path.stat().st_mtime_ns), (original.st_ino, original.st_mtime_ns))
        self.n.stop(signal.SIGKILL)
        self.verify_stopped()
        self.n.start()
        self.assertEqual(self.n.call("Schedule.Create", request), complete)
        self.assertEqual(self.read_workflow(first_run), after_deadline)
        self.assertEqual(self.budget(), (0, 2))
