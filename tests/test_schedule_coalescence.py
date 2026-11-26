"""Exercise timed occurrence coalescence through the real Unix authority."""

from contextlib import closing
from datetime import datetime, timedelta
from hashlib import sha256
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Node, key, target
from test_schedule import after, timestamp


class ScheduleCoalescence(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.grant = self.n.call("Grant.Issue", {"principal_id": "local-owner", "units": 6, "ttl_seconds": 120,
            "actions": ["schedule.manage", "task.submit", "task.read", "system.status"],
            "tool_targets": ["profile-local"]})

    def new_schedule(self, *, interval, window, count=4):
        start = timestamp(1)
        end = (datetime.fromisoformat(start) + timedelta(seconds=window)).isoformat(timespec="milliseconds")
        request = {"key": key(), "tool": "system.status", "input": {}, "run_at": start, "deadline_at": end,
            "repeat": {"interval_seconds": interval, "count": count, "missed": {"mode": "Coalesce"}, "overlap": "Allow"},
            "delegation_id": self.grant["delegation_ref"]["resource_id"]}
        return request, self.n.call("Schedule.Create", request)

    def set_scheduling(self, mode):
        node = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {**target(node), "scheduling": mode})

    def state(self, schedule):
        return self.n.call("Schedule.Get", {"id": schedule["reference"]["resource_id"]})

    def ids(self, kind):
        return {item["resource_id"] for item in self.n.call("Resource.List", {"kind": kind})["items"]}

    def budget(self):
        budget = self.n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]})
        return budget["reserved"], budget["spent"]

    def until(self, read, ready):
        limit = time.monotonic() + 12
        while True:
            value = read()
            if ready(value):
                return value
            self.assertLess(time.monotonic(), limit, value)
            time.sleep(0.04)

    def deferral(self, schedule):
        with closing(sqlite3.connect((self.n.state / "state.db").as_uri() + "?mode=ro", uri=True, timeout=5)) as db:
            db.row_factory = sqlite3.Row
            row = db.execute("""SELECT d.kind,d.trigger,d.anchor,d.schedule_rev,d.deferred_boot_ms,
                       p.run_ms,p.deadline_ms,elapsed.deadline_boot_ms
                FROM schedule_admission_deferrals d JOIN pending_schedules p ON p.schedule=d.schedule
                JOIN authorization_deadlines elapsed ON elapsed.resource=d.schedule
                WHERE d.schedule=?""", (schedule["reference"]["resource_id"],)).fetchone()
        return dict(row) if row is not None else None

    def triggers(self, schedule):
        reports = []
        for reference in schedule["state"]["trigger_refs"]:
            row = self.n.call("Schedule.TriggerGet", {"id": reference["resource_id"]})
            self.assertEqual(row["reference"], reference)
            reports.append(row["state"])
        return reports

    def complete(self, schedule):
        return self.until(lambda: self.state(schedule), lambda report: report["state"]["lifecycle"] == "Completed")

    def check_group(self, previous, final, request, *, expired):
        state = final["state"]
        actual = self.triggers(final)
        self.assertEqual(len(actual), request["repeat"]["count"])
        self.assertEqual(state["lifecycle"], "Completed", state)
        self.assertIsNone(state["next"])
        schedule_id = final["reference"]["resource_id"]
        references = [ref["resource_id"] for ref in state["trigger_refs"]]
        start, end = [datetime.fromisoformat(request[field]) for field in ("run_at", "deadline_at")]
        for index, report in enumerate(actual):
            shift = timedelta(seconds=index * request["repeat"]["interval_seconds"])
            self.assertEqual(report["ordinal"], index + 1)
            self.assertEqual(report["schedule_id"], schedule_id)
            self.assertEqual(datetime.fromisoformat(report["run_at"]), start + shift)
            self.assertEqual(datetime.fromisoformat(report["deadline_at"]), end + shift)
            identity = json.dumps(["psios/schedule/trigger/1", schedule_id, "r1", report["run_at"]],
                                  separators=(",", ":")).encode()
            self.assertEqual(references[index], "trigger-" + sha256(identity).hexdigest())

        representative = actual[-1]
        self.assertEqual((representative["lifecycle"], representative["reason"]), ("Triggered", None), representative)
        self.assertNotIn("coalesced_into_trigger_id", representative)
        proof = representative["coalescence"]
        self.assertEqual(proof["covered_trigger_ids"], references[expired:-1])
        self.assertEqual(proof["schedule_ref"], previous["reference"])
        selected = datetime.fromisoformat(proof["selected_at"])
        self.assertGreaterEqual(selected, datetime.fromisoformat(representative["run_at"]))
        self.assertLess(selected, datetime.fromisoformat(representative["deadline_at"]))
        for report in actual[:expired]:
            self.assertEqual((report["lifecycle"], report["reason"], report["task_receipt"]),
                             ("Expired", "DeadlineExceeded", None), report)
            self.assertNotIn("coalescence", report)
            self.assertNotIn("coalesced_into_trigger_id", report)
            self.assertLessEqual(datetime.fromisoformat(report["deadline_at"]), selected)
        for report in actual[expired:-1]:
            self.assertEqual((report["lifecycle"], report["reason"], report["task_receipt"]),
                             ("Skipped", "Coalesced", None), report)
            self.assertEqual(report["coalesced_into_trigger_id"], references[-1], report)
            self.assertNotIn("coalescence", report)
            self.assertLess(selected, datetime.fromisoformat(report["deadline_at"]))

        receipt = representative["task_receipt"]
        self.assertEqual(receipt["idempotency_key"], references[-1])
        self.assertEqual((receipt["origin_ref"]["origin_type"], receipt["origin_ref"]["origin_id"]),
                         ("Schedule", schedule_id))
        task = self.n.wait(receipt["task_id"])
        self.assertEqual(task["origin_ref"], receipt["origin_ref"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        output = self.n.call("Artifact.Read", {"reference": artifact})
        self.assertEqual(output["authority_id"], self.n.call("Principal.WhoAmI", {})["authority_id"])
        self.assertEqual(state["last_task_id"], task["task_id"])
        return task, actual

    def verify_stopped(self):
        report = json.loads(subprocess.check_output(self.n.daemon + ["verify"], timeout=20))
        self.assertEqual(report["sqlite_integrity"], "Verified", report)
        self.assertFalse(report["migration_required"], report)

    def reject_expired_member_in_group(self, completed):
        expired, *_, representative = [ref["resource_id"] for ref in completed["state"]["trigger_refs"]]
        database = self.n.state / "state.db"
        with closing(sqlite3.connect(database, timeout=10)) as db, db:
            original, = db.execute("SELECT body FROM versions WHERE id=? AND rev=1", (representative,)).fetchone()
            changed = json.loads(original)
            changed["coalescence"]["covered_trigger_ids"][0] = expired
            db.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (json.dumps(changed), representative))
        try:
            rejected = subprocess.run(self.n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
            self.assertNotEqual(rejected.returncode, 0, rejected)
            self.assertIn("RecoveryRequired", rejected.stderr)
        finally:
            with closing(sqlite3.connect(database, timeout=10)) as db, db:
                db.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (original, representative))

    def test_cordoned_recovery_marks_expired_windows_and_dispatches_latest_eligible_once(self):
        n = self.n
        self.set_scheduling("Cordoned")
        request, schedule = self.new_schedule(interval=3, window=8.5)
        after(request["run_at"], 9.16)
        before = self.until(lambda: self.state(schedule), lambda state: len(state["state"]["trigger_refs"]) >= 1)
        self.assertEqual(len(before["state"]["trigger_refs"]), 1, before)
        self.assertEqual(before["state"]["trigger_refs"][0]["resource_id"], schedule["state"]["next"]["trigger_id"])
        self.assertEqual(before["state"]["next"]["ordinal"], 2)
        self.assertEqual(self.budget(), (0, 0))
        self.assertFalse(self.ids("Task"))
        self.set_scheduling("Enabled")
        final = self.complete(schedule)
        task, triggers = self.check_group(before, final, request, expired=1)
        self.assertEqual(self.ids("Task"), {task["task_id"]})
        self.assertEqual(self.budget(), (0, 1))
        n.stop(signal.SIGKILL)
        self.verify_stopped()
        self.reject_expired_member_in_group(final)
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), final)
        self.assertEqual(self.triggers(final), triggers)
        self.assertEqual(self.ids("Task"), {task["task_id"]})
        self.assertEqual(self.budget(), (0, 1))

    def test_full_queue_retains_uncommitted_group_through_restart_and_uses_latest_due(self):
        n = self.n
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "coalescence-queue-capacity"})
        held = n.submit("volume.write", {**target(volume), "name": "held", "content": "must remain absent"})
        waiting = n.wait(held)
        self.assertEqual(waiting["lifecycle"], "WaitingApproval")
        path = n.state / "volumes" / volume["resource_id"] / "data" / "held"
        initial_tasks = self.ids("Task")
        request, initial = self.new_schedule(interval=2, window=25)
        self.assertFalse(initial["state"]["trigger_refs"])
        deferred = self.until(lambda: self.deferral(initial), lambda row: row is not None)
        self.assertEqual((deferred["kind"], deferred["trigger"], deferred["schedule_rev"]),
                         ("Timed", initial["state"]["next"]["trigger_id"], 1), deferred)
        self.assertEqual(deferred["anchor"], deferred["run_ms"])
        self.assertEqual(self.state(initial), initial)
        self.assertEqual(self.ids("Task"), initial_tasks)
        self.assertFalse(self.ids("ScheduleTrigger"))
        self.assertEqual(self.budget(), (0, 0))
        n.stop(signal.SIGKILL)
        self.verify_stopped()
        # The final original window becomes eligible while the authority is stopped.
        after(request["run_at"], 6.16)
        n.start()
        # Force the newest possible group to attempt admission while the queue is still full.
        retry = self.until(lambda: self.deferral(initial),
                           lambda row: row is not None and row["deferred_boot_ms"] > deferred["deferred_boot_ms"])
        self.assertEqual(retry, {**deferred, "deferred_boot_ms": retry["deferred_boot_ms"]})
        self.assertEqual(self.state(initial), initial)
        self.assertEqual(n.wait(held), waiting)
        self.assertEqual(self.ids("Task"), initial_tasks)
        self.assertFalse(self.ids("ScheduleTrigger"))
        self.assertEqual(self.budget(), (0, 0))

        cancelled = n.call("Task.Cancel", target(n.get(held)["reference"]))
        self.assertEqual(cancelled["lifecycle"], "Cancelled")
        final = self.complete(initial)
        task, triggers = self.check_group(initial, final, request, expired=0)
        self.assertIsNone(self.deferral(initial))
        self.assertEqual(self.ids("Task"), initial_tasks | {task["task_id"]})
        self.assertEqual(self.budget(), (0, 1))
        self.assertFalse(path.exists())
        n.stop(signal.SIGKILL)
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), final)
        self.assertEqual(self.triggers(final), triggers)
        self.assertEqual(self.ids("Task"), initial_tasks | {task["task_id"]})
        self.assertEqual(self.budget(), (0, 1))
