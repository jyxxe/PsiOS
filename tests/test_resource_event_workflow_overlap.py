"""Exercise event-triggered Workflow custody through Unix and real Volume writes."""

from contextlib import closing, contextmanager
from datetime import datetime
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest
import uuid

from support import Failure, Node, key, target
from test_schedule import timestamp


CURSOR_ERROR = "protected Workflow event schedule disagrees with its pending cursor marker"
CLOCK_ERROR = "protected Workflow event schedule lost its original elapsed cutoff"
SUBMISSION_ERROR = "protected Workflow event trigger lost its authoritative Task submission"


class ResourceEventWorkflowOverlap(unittest.TestCase):
    payload = b"Written by the event-triggered Workflow.\n"

    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "event-workflow-overlap"})
        self.file = self.n.state / "volumes" / self.volume["resource_id"] / "data" / "event-proof"
        self.n.call("Grant.Issue", {"principal_id": "local-owner", "units": 1, "ttl_seconds": 120,
            "actions": ["workflow.manage"], "tool_targets": [self.volume["resource_id"]]})
        self.definition = self.n.call("Workflow.Define", {"key": key(), "steps": [{
            "id": "record", "tool": "volume.write", "after": [],
            "input": {**target(self.volume), "name": self.file.name, "content": self.payload.decode()},
        }]})["reference"]
        self.scope = self.n.call("Grant.Issue", {"principal_id": "local-owner", "units": 8, "ttl_seconds": 120,
            "actions": ["schedule.manage", "workflow.manage", "workflow.start", "task.submit", "task.read",
                        "event.read", "resource.read", "volume.write", "system.status"],
            "tool_targets": [self.definition["resource_id"], self.volume["resource_id"], "profile-local"]})
        self.initial_tasks = self.ids("Task")
        self.source = self.n.call("Node.Get", {})["reference"]
        self.cursor = self.n.call("Resource.List", {"kind": "Node", "id": self.source["resource_id"]})[
            "watch_start_cursor"]

    def ids(self, kind):
        return {ref["resource_id"] for ref in self.n.call("Resource.List", {"kind": kind})["items"]}

    def budget(self):
        budget = self.n.call("Budget.Get", {"id": self.scope["budget_ref"]["resource_id"]})
        return budget["reserved"], budget["spent"]

    def until(self, read, ready):
        deadline = time.monotonic() + 12
        while True:
            report = read()
            if ready(report):
                return report
            self.assertLess(time.monotonic(), deadline, report)
            time.sleep(0.04)

    def create(self, *, count, max_runtime):
        request = {"key": key(), "tool": "workflow.start",
            "input": {"definition_ref": self.definition, "max_runtime_seconds": max_runtime},
            "delegation_id": self.scope["delegation_ref"]["resource_id"],
            "event": {"source_ref": self.source, "condition": {"kind": "SpecChanged"},
                      "expires_at": timestamp(90), "max_triggers": count, "overlap": "SkipWhileWorkflowActive"}}
        created = self.n.call("Schedule.Create", request)
        schedule_id = created["reference"]["resource_id"]
        spec = self.n.get(schedule_id)["resource"]["spec"]
        self.assertEqual(spec["event"]["overlap"], "SkipWhileWorkflowActive")
        self.assertEqual(created["state"]["source_ref"], self.source)
        self.assertEqual(created["state"]["lifecycle"], "Pending")
        self.assertFalse(created["state"]["trigger_refs"])
        return request, schedule_id

    def stop(self):
        self.n.stop(signal.SIGKILL)

    def verify_stopped(self):
        report = json.loads(subprocess.check_output(self.n.daemon + ["verify"], timeout=20))
        self.assertEqual(report["sqlite_integrity"], "Verified", report)
        self.assertFalse(report["migration_required"], report)

    def reject_persisted_change(self, change, restore, message, *, startup=True):
        self.assertIsNotNone(self.n.process.poll())
        path = self.n.state / "state.db"
        with closing(sqlite3.connect(path, timeout=10)) as db, db:
            self.assertEqual(db.execute(*change).rowcount, 1)
        try:
            commands = ("verify", "serve") if startup else ("verify",)
            for command in commands:
                denied = subprocess.run(self.n.daemon + [command], capture_output=True, text=True, timeout=20)
                self.assertNotEqual(denied.returncode, 0, denied)
                self.assertIn("RecoveryRequired", denied.stderr)
                self.assertIn(message, denied.stderr)
        finally:
            with closing(sqlite3.connect(path, timeout=10)) as db, db:
                self.assertEqual(db.execute(*restore).rowcount, 1)

    def change_source(self, queue_limit):
        n = self.n
        self.assertIsNotNone(n.process.poll())
        cfg = json.loads(n.config.read_text())
        self.assertNotEqual(cfg["queue_limit"], queue_limit)
        n.config.write_text(json.dumps({**cfg, "queue_limit": queue_limit}))
        n.start()
        current = n.call("Node.Get", {})
        self.assertEqual(current["state"]["configuration"]["queue_limit"], queue_limit)
        self.assertEqual(current["reference"]["resource_id"], self.source["resource_id"])
        return current["reference"]

    def get_schedule(self, schedule_id):
        return self.n.call("Schedule.Get", {"id": schedule_id})

    def queue_blocker(self):
        other = self.n.perform("volume.create", {"name": "event-workflow-admission-blocker"})
        blocker = self.n.submit("volume.write", {**target(other), "name": "held", "content": "not written"})
        held = self.n.wait(blocker)
        self.assertEqual((held["lifecycle"], held["outcome_knowledge"]), ("WaitingApproval", "NotStarted"), held)
        held_file = self.n.state / "volumes" / other["resource_id"] / "data" / "held"
        return held, held_file

    def pending_event(self, schedule_id):
        deferred = self.until(lambda: self.get_schedule(schedule_id),
                              lambda row: row["state"]["pending_trigger_ref"] is not None)
        pending_ref = deferred["state"]["pending_trigger_ref"]
        self.assertEqual(pending_ref["version"], "r1")
        pending = self.n.call("Schedule.TriggerGet", {"id": pending_ref["resource_id"]})
        self.assertEqual(pending["reference"], pending_ref)
        self.assertEqual((pending["state"]["lifecycle"], pending["state"]["task_receipt"]), ("Pending", None))
        self.assertEqual(deferred["state"]["source_ref"], self.source)
        self.assertFalse(deferred["state"]["trigger_refs"])
        return deferred, pending

    def wait_triggers(self, schedule_id, count):
        schedule = self.until(lambda: self.get_schedule(schedule_id),
                              lambda row: len(row["state"]["trigger_refs"]) >= count)
        self.assertEqual(len(schedule["state"]["trigger_refs"]), count, schedule)
        self.assertIsNone(schedule["state"]["pending_trigger_ref"])
        triggers = [self.n.call("Schedule.TriggerGet", {"id": ref["resource_id"]})
                    for ref in schedule["state"]["trigger_refs"]]
        self.assertEqual([row["reference"] for row in triggers], schedule["state"]["trigger_refs"])
        return schedule, triggers

    def assert_source_events(self, triggers, references):
        watched = self.n.call("Event.Poll", {"cursor": self.cursor})["events"]
        self.assertEqual([row["resource_ref"] for row in watched], references, watched)
        self.assertEqual(len(triggers), len(watched))
        fields = ("event_id", "source_seq", "source_authority_id", "recovery_incarnation",
                  "resource_ref", "occurred_at", "event_type")
        for trigger, event in zip(triggers, watched):
            source_event = trigger["state"]["source_event"]
            self.assertEqual(source_event, {field: event[field] for field in fields})
            self.assertEqual(source_event["event_type"], "psios.node.updated.v1")
            self.assertEqual(trigger["state"]["trigger_id"], trigger["reference"]["resource_id"])
        self.assertEqual(len({row["reference"]["resource_id"] for row in triggers}), len(triggers))

    def workflow(self, run_id):
        return self.n.call("Workflow.Get", {"id": run_id})

    def wait_workflow(self, run_id, lifecycle):
        return self.until(lambda: self.workflow(run_id), lambda row: row["state"]["lifecycle"] == lifecycle)

    def launch(self, trigger):
        state = trigger["state"]
        self.assertEqual(state["lifecycle"], "Triggered", trigger)
        self.assertNotIn("overlap_evidence", state)
        receipt = state["task_receipt"]
        self.assertEqual(receipt["idempotency_key"], state["trigger_id"])
        launch = self.n.wait(receipt["task_id"])
        self.assertEqual((launch["lifecycle"], launch["outcome_knowledge"]), ("Succeeded", "Known"), launch)
        self.assertEqual(launch["origin_ref"], receipt["origin_ref"])
        self.assertEqual((launch["origin_ref"]["origin_type"], launch["origin_ref"]["origin_id"]),
                         ("Schedule", state["schedule_id"]))
        artifact, = launch["result"]["artifact_refs"]
        launch_result = self.n.call("Artifact.Read", {"reference": artifact})
        self.assertEqual(launch_result["definition_ref"], self.definition)
        self.assertEqual(launch_result["origin_task_ref"]["resource_id"], launch["task_id"])
        run_id = launch_result["workflow_run_ref"]["resource_id"]
        workflow = self.wait_workflow(run_id, "WaitingApproval")
        self.assertEqual(workflow["state"]["origin_task_ref"], launch_result["origin_task_ref"])
        step, = workflow["state"]["steps"]
        self.assertEqual(step["step_id"], "record")
        child = self.n.wait(step["task_receipt"]["task_id"])
        self.assertEqual((child["lifecycle"], child["outcome_knowledge"]), ("WaitingApproval", "NotStarted"), child)
        self.assertEqual((child["origin_ref"]["origin_type"], child["origin_ref"]["origin_id"]),
                         ("WorkflowRun", run_id))
        return launch["task_id"], workflow, child

    def assert_skip(self, trigger, first, launch_id, workflow):
        state = trigger["state"]
        self.assertEqual((state["lifecycle"], state["reason"], state["task_receipt"]),
                         ("Skipped", "PreviousWorkflowActive", None), state)
        artifact, = self.n.wait(launch_id)["result"]["artifact_refs"]
        self.assertEqual(state["overlap_evidence"], {"launch_trigger_ref": first["reference"],
            "launch_task_ref": self.n.get(launch_id)["reference"], "launch_artifact_ref": artifact,
            "workflow_run_ref": workflow["reference"]})
        resolved = self.n.call("Resource.Resolve", {"reference": workflow["reference"]})
        self.assertEqual(resolved["reference"], workflow["reference"])
        self.assertEqual(resolved["value"]["origin_task_ref"], workflow["state"]["origin_task_ref"])
        self.assertEqual(resolved["value"]["lifecycle"], workflow["state"]["lifecycle"])

    @contextmanager
    def withhold_write_receipt(self, task_id):
        path = self.n.state / "state.db"
        with closing(sqlite3.connect(path, timeout=10)) as db, db:
            db.execute(f"""CREATE TRIGGER missing_event_workflow_write_receipt BEFORE INSERT ON artifact_chunks
                WHEN EXISTS(SELECT 1 FROM versions WHERE id=NEW.artifact_id AND spec->>'task_id'='{task_id}')
                BEGIN SELECT RAISE(ABORT, 'fixture withheld the actual write receipt'); END""")
        try:
            yield
        finally:
            with closing(sqlite3.connect(path, timeout=10)) as db, db:
                db.execute("DROP TRIGGER missing_event_workflow_write_receipt")

    def test_approval_wait_survives_restart_and_consumes_event_without_a_second_launch(self):
        n = self.n
        request, schedule_id = self.create(count=3, max_runtime=60)
        with self.assertRaises(Failure) as invalid:
            n.call("Schedule.Create", {**request, "key": key(), "tool": "system.status", "input": {}})
        self.assertEqual(invalid.exception.code, "InvalidArgument", invalid.exception)

        self.stop()
        first_source = self.change_source(63)
        _, (first,) = self.wait_triggers(schedule_id, 1)
        launch_id, waiting, child = self.launch(first)
        first_run = waiting["reference"]["resource_id"]
        first_tasks = self.initial_tasks | {launch_id, child["task_id"]}
        self.assertEqual(self.ids("Task"), first_tasks)
        self.assertEqual(self.ids("WorkflowRun"), {first_run})
        self.assertEqual(self.budget(), (1, 1))
        self.assertFalse(self.file.exists())

        self.stop()
        self.verify_stopped()
        second_source = self.change_source(62)
        pending, triggers = self.wait_triggers(schedule_id, 2)
        self.assertEqual(triggers[0], first)
        self.assertEqual(self.workflow(first_run), waiting)
        self.assert_skip(triggers[1], first, launch_id, waiting)
        self.assert_source_events(triggers, [first_source, second_source])
        self.assertEqual(pending["state"]["source_ref"], second_source)
        self.assertEqual(pending["state"]["last_task_id"], launch_id)
        self.assertEqual(pending["state"]["lifecycle"], "Pending")
        self.assertEqual(n.wait(child["task_id"]), child)
        self.assertEqual(self.ids("Task"), first_tasks)
        self.assertEqual(self.ids("WorkflowRun"), {first_run})
        self.assertEqual(self.budget(), (1, 1))
        self.assertFalse(self.file.exists())

        cancelled = n.call("Workflow.Cancel", target(waiting["reference"]))
        self.assertEqual(cancelled["state"]["lifecycle"], "Cancelled", cancelled)
        self.assertEqual(n.wait(child["task_id"])["lifecycle"], "Cancelled")
        self.assertEqual(n.get(self.volume["resource_id"])["reference"], self.volume)
        self.assertEqual(self.budget(), (0, 1))
        self.assertFalse(self.file.exists())

        self.stop()
        third_source = self.change_source(61)
        complete, final_triggers = self.wait_triggers(schedule_id, 3)
        self.assertEqual(final_triggers[:2], triggers)
        self.assert_source_events(final_triggers, [first_source, second_source, third_source])
        self.assertEqual(complete["state"]["source_ref"], third_source)
        self.assertEqual(complete["state"]["lifecycle"], "Completed")
        last_launch, last_workflow, last_child = self.launch(final_triggers[-1])
        last_run = last_workflow["reference"]["resource_id"]
        self.assertNotEqual(last_run, first_run)
        self.assertEqual(complete["state"]["last_task_id"], last_launch)
        final_tasks = first_tasks | {last_launch, last_child["task_id"]}
        self.assertEqual(self.ids("Task"), final_tasks)
        self.assertEqual(self.ids("WorkflowRun"), {first_run, last_run})
        self.assertEqual(self.budget(), (1, 2))
        self.assertFalse(self.file.exists())
        n.approve(last_child["task_id"])
        succeeded = self.wait_workflow(last_run, "Succeeded")
        self.assertEqual(self.file.read_bytes(), self.payload)
        file = self.file.stat()
        self.assertEqual(self.budget(), (0, 3))

        self.stop()
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), complete)
        self.assertEqual(self.wait_triggers(schedule_id, 3), (complete, final_triggers))
        self.assertEqual(self.workflow(last_run), succeeded)
        self.assertEqual(self.ids("Task"), final_tasks)
        self.assertEqual(self.budget(), (0, 3))
        self.assertEqual((self.file.stat().st_ino, self.file.stat().st_mtime_ns), (file.st_ino, file.st_mtime_ns))

    def test_waiting_event_keeps_original_revision_and_skip_points_to_its_settlement(self):
        n = self.n
        held, held_file = self.queue_blocker()
        blocker = held["task_id"]
        baseline_tasks = self.ids("Task")
        request, schedule_id = self.create(count=2, max_runtime=60)

        self.stop()
        first_source = self.change_source(1)
        deferred, pending = self.pending_event(schedule_id)
        pending_ref = pending["reference"]
        self.assert_source_events([pending], [first_source])
        self.assertEqual(self.ids("Task"), baseline_tasks)
        self.assertFalse(self.ids("WorkflowRun"))
        self.assertEqual(self.budget(), (0, 0))

        self.stop()
        self.verify_stopped()
        n.start()
        self.assertEqual(self.get_schedule(schedule_id), deferred)
        self.assertEqual(n.call("Schedule.TriggerGet", {"id": pending_ref["resource_id"]}), pending)
        self.assertEqual(n.wait(blocker), held)
        n.call("Task.Cancel", {"id": blocker, "expected_resource_version": held["task_revision"]})
        self.assertEqual(n.wait(blocker)["lifecycle"], "Cancelled")

        settled, (first,) = self.wait_triggers(schedule_id, 1)
        self.assertEqual(first["reference"]["resource_id"], pending_ref["resource_id"])
        self.assertEqual(first["reference"]["version"], "r2")
        self.assertEqual(first["state"]["source_event"], pending["state"]["source_event"])
        self.assertEqual(settled["state"]["source_ref"], first_source)
        launch_id, waiting, child = self.launch(first)
        expected_tasks = baseline_tasks | {launch_id, child["task_id"]}
        self.assertEqual(self.ids("Task"), expected_tasks)
        self.assertEqual(self.budget(), (1, 1))

        self.stop()
        self.verify_stopped()
        second_source = self.change_source(2)
        complete, triggers = self.wait_triggers(schedule_id, 2)
        self.assertEqual(triggers[0], first)
        self.assert_skip(triggers[1], first, launch_id, waiting)
        self.assert_source_events(triggers, [first_source, second_source])
        self.assertEqual(complete["state"]["lifecycle"], "Completed")
        self.assertEqual(complete["state"]["source_ref"], second_source)
        self.assertEqual(complete["state"]["last_task_id"], launch_id)
        self.assertEqual(self.ids("Task"), expected_tasks)
        self.assertEqual(self.ids("WorkflowRun"), {waiting["reference"]["resource_id"]})
        self.assertEqual(n.wait(child["task_id"]), child)
        self.assertEqual(self.budget(), (1, 1))
        self.assertFalse(held_file.exists())
        self.assertFalse(self.file.exists())

        self.stop()
        self.verify_stopped()
        self.reject_changed_pending_source(pending_ref, triggers[1]["state"]["source_event"]["source_seq"])
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db:
            digest, scope = db.execute("SELECT digest,scope FROM submissions WHERE task=?", (launch_id,)).fetchone()
        self.assertEqual(scope, schedule_id)
        self.assertEqual(digest, first["state"]["task_receipt"]["request_digest"])
        change_digest = "UPDATE submissions SET digest=? WHERE task=?"
        self.reject_persisted_change((change_digest, ("sha256:" + "0" * 64, launch_id)),
                                 (change_digest, (digest, launch_id)), SUBMISSION_ERROR, startup=False)
        change_scope = "UPDATE submissions SET scope=? WHERE task=?"
        self.reject_persisted_change((change_scope, (self.source["resource_id"], launch_id)),
                                 (change_scope, (scope, launch_id)), SUBMISSION_ERROR, startup=False)
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), complete)
        self.assertEqual(self.wait_triggers(schedule_id, 2), (complete, triggers))
        self.assertEqual(self.ids("Task"), expected_tasks)
        self.assertEqual(self.budget(), (1, 1))

    def reject_changed_pending_source(self, reference, later_source_seq):
        path = self.n.state / "state.db"
        location = (reference["resource_id"], 1)
        with closing(sqlite3.connect(path, timeout=10)) as db, db:
            raw, = db.execute("SELECT body FROM versions WHERE id=? AND rev=?", location).fetchone()
            changed = json.loads(raw)
            self.assertNotEqual(changed["source_event"]["source_seq"], later_source_seq)
            changed["source_event"]["source_seq"] = later_source_seq
            db.execute("UPDATE versions SET body=? WHERE id=? AND rev=?", (json.dumps(changed), *location))
        try:
            denied = subprocess.run(self.n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
            self.assertNotEqual(denied.returncode, 0, denied)
            self.assertIn("RecoveryRequired", denied.stderr)
            self.assertIn("protected Workflow event schedule has inconsistent source custody", denied.stderr)
        finally:
            with closing(sqlite3.connect(path, timeout=10)) as db, db:
                db.execute("UPDATE versions SET body=? WHERE id=? AND rev=?", (raw, *location))

    def test_protected_cursor_and_clock_refuse_drift_then_original_pending_event_cancels(self):
        n = self.n
        held, held_file = self.queue_blocker()
        baseline_tasks = self.ids("Task")
        request, schedule_id = self.create(count=2, max_runtime=60)
        opening = n.call("Node.WorkAdmission.Get", {})["reference"]
        draining = n.call("Node.WorkAdmission.Set", {"expected_resource_version": opening["version"],
            "mode": "Draining", "reason": "keep a real new source event unexamined"})

        self.stop()
        source = self.change_source(1)
        self.assertFalse(self.get_schedule(schedule_id)["state"]["trigger_refs"])
        self.assertIsNone(self.get_schedule(schedule_id)["state"]["pending_trigger_ref"])
        self.stop()
        self.verify_stopped()

        pending_table = "pending_event_schedules"
        path = n.state / "state.db"
        with closing(sqlite3.connect(path, timeout=10)) as db:
            index = db.execute("SELECT schedule,source,source_rev,deadline_ms FROM pending_event_schedules "
                               "WHERE schedule=?", (schedule_id,)).fetchone()
        self.assertEqual(index[:3], (schedule_id, self.source["resource_id"], int(self.source["version"][1:])))
        advanced = int(source["version"][1:])
        self.assertEqual(advanced, index[2] + 1)
        change_cursor = f"UPDATE {pending_table} SET source_rev=? WHERE schedule=?"
        self.reject_persisted_change((change_cursor, (advanced, schedule_id)),
                                 (change_cursor, (index[2], schedule_id)), CURSOR_ERROR)
        # Remove the index while no admission deferral exists to test the protected marker alone.
        self.reject_persisted_change((f"DELETE FROM {pending_table} WHERE schedule=?", (schedule_id,)),
                                 (f"INSERT INTO {pending_table}(schedule,source,source_rev,deadline_ms) VALUES(?,?,?,?)",
                                  index), CURSOR_ERROR)
        self.verify_stopped()

        n.start()
        self.assertEqual(n.call("Node.Get", {})["reference"], source)
        n.call("Node.WorkAdmission.Set", {"expected_resource_version": draining["reference"]["version"],
            "mode": "Open", "reason": "let the full queue park its source event"})
        deferred, pending = self.pending_event(schedule_id)
        self.assert_source_events([pending], [source])
        self.assertEqual(self.ids("Task"), baseline_tasks)
        self.assertEqual(self.budget(), (0, 0))

        self.stop()
        self.verify_stopped()
        with closing(sqlite3.connect(path, timeout=10)) as db:
            original_boot, = db.execute("SELECT boot_id FROM authorization_deadlines WHERE resource=?",
                                        (schedule_id,)).fetchone()
        change_boot = "UPDATE authorization_deadlines SET boot_id=? WHERE resource=?"
        self.reject_persisted_change((change_boot, (str(uuid.uuid4()), schedule_id)),
                                 (change_boot, (original_boot, schedule_id)), CLOCK_ERROR)
        self.verify_stopped()

        n.start()
        self.assertEqual(self.pending_event(schedule_id), (deferred, pending))
        cancelled = n.call("Schedule.Cancel", target(deferred["reference"]))
        self.assertEqual(cancelled["state"]["lifecycle"], "Cancelled")
        self.assertEqual(cancelled["state"]["source_ref"], self.source)
        self.assertIsNone(cancelled["state"]["pending_trigger_ref"])
        _, (closed,) = self.wait_triggers(schedule_id, 1)
        self.assertEqual(closed["reference"]["resource_id"], pending["reference"]["resource_id"])
        self.assertEqual(closed["reference"]["version"], "r2")
        self.assertEqual((closed["state"]["lifecycle"], closed["state"]["reason"], closed["state"]["task_receipt"]),
                         ("Cancelled", None, None))
        self.assertEqual(closed["state"]["source_event"], pending["state"]["source_event"])
        self.assertNotIn("overlap_evidence", closed["state"])
        self.stop()
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), cancelled)
        self.assertEqual(self.wait_triggers(schedule_id, 1), (cancelled, [closed]))
        self.assertEqual(self.ids("Task"), baseline_tasks)
        self.assertFalse(self.ids("WorkflowRun"))
        self.assertEqual(self.budget(), (0, 0))
        self.assertEqual(n.wait(held["task_id"]), held)
        self.assertFalse(held_file.exists())
        self.assertFalse(self.file.exists())

    def test_unknown_physical_write_still_blocks_after_child_workflow_deadline(self):
        request, schedule_id = self.create(count=2, max_runtime=8)
        self.stop()
        first_source = self.change_source(63)
        _, (first,) = self.wait_triggers(schedule_id, 1)
        launch_id, waiting, child = self.launch(first)
        run_id = waiting["reference"]["resource_id"]
        with self.withhold_write_receipt(child["task_id"]):
            self.n.approve(child["task_id"])
            unknown_task = self.n.wait(child["task_id"])
            self.assertEqual((unknown_task["lifecycle"], unknown_task["outcome_knowledge"]),
                             ("Paused", "Unknown"), unknown_task)
            blocked = self.wait_workflow(run_id, "Blocked")
            self.assertEqual((blocked["state"]["reason"], blocked["state"]["outcome_knowledge"]),
                             ("OutcomeUnknown", "Unknown"), blocked)
            self.assertEqual(self.file.read_bytes(), self.payload)
            file = self.file.stat()
            self.assertEqual(self.budget(), (0, 2))
            self.stop()
        self.verify_stopped()
        self.n.start()
        expired = self.until(lambda: self.workflow(run_id),
                             lambda row: row["state"]["stop_reason"] == "DeadlineExceeded")
        self.assertEqual((expired["state"]["lifecycle"], expired["state"]["outcome_knowledge"]),
                         ("Blocked", "Unknown"), expired)
        self.assertEqual(self.wait_triggers(schedule_id, 1)[1], [first])

        self.stop()
        second_source = self.change_source(62)
        complete, triggers = self.wait_triggers(schedule_id, 2)
        self.assertEqual(triggers[0], first)
        self.assert_skip(triggers[1], first, launch_id, expired)
        self.assert_source_events(triggers, [first_source, second_source])
        skipped_at = datetime.fromisoformat(triggers[1]["state"]["source_event"]["occurred_at"])
        self.assertGreater(skipped_at, datetime.fromisoformat(waiting["deadline_at"]))
        self.assertLess(skipped_at, datetime.fromisoformat(request["event"]["expires_at"]))
        self.assertEqual(complete["state"]["lifecycle"], "Completed")
        self.assertEqual(complete["state"]["last_task_id"], launch_id)
        self.assertEqual(complete["state"]["source_ref"], second_source)
        self.assertEqual(self.workflow(run_id), expired)
        self.assertEqual(self.n.wait(child["task_id"]), unknown_task)
        self.assertEqual(self.ids("Task"), self.initial_tasks | {launch_id, child["task_id"]})
        self.assertEqual(self.ids("WorkflowRun"), {run_id})
        self.assertEqual(self.budget(), (0, 2))
        self.assertEqual(self.file.read_bytes(), self.payload)
        self.assertEqual((self.file.stat().st_ino, self.file.stat().st_mtime_ns), (file.st_ino, file.st_mtime_ns))
        self.stop()
        self.verify_stopped()
