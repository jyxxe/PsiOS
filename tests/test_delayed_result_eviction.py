"""Exercise delayed result eviction with a real Unix authority and task artifacts."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
import signal
import sqlite3
import subprocess
from threading import Barrier
import time
import unittest

from support import Failure, Node, key, target
from test_task_result_eviction_batch import APPLY, PREPARE, Observation


SCHEDULE = "Task.ScheduleResultEvictionBatch"
GET = "Task.GetResultEvictionSchedule"
CANCEL = "Task.CancelResultEvictionSchedule"
DELAY = 4


def boottime():
    return time.clock_gettime(time.CLOCK_BOOTTIME)


def after(deadline):
    while (remaining := deadline - boottime()) > 0:
        time.sleep(remaining)


class DelayedResultEvictionTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.contributor = self.node.call("Agent.Register", {"name": "delayed-result-contributor"})
        self.token = self.contributor["token"]
        self.profile = self.node.get("profile-local")["reference"]
        self.execution = self.grant(
            self.contributor["principal_id"], ["task.submit", "task.read", "system.status"],
            [self.profile["resource_id"]],
        )["delegation_ref"]["resource_id"]

    def grant(self, principal, actions, targets):
        return self.node.call("Grant.Issue", {
            "principal_id": principal, "actions": actions, "tool_targets": targets,
            "units": 8, "ttl_seconds": 300,
        })

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as caught:
            self.node.call(method, params, token)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def observe(self):
        n = self.node
        request = n.prepare("system.status", {}, token=self.token, delegation=self.execution)
        task_id = n.call("Task.Submit", request, self.token)["task_id"]
        state = n.wait(task_id)
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"]), ("Succeeded", "Known"), state)
        artifact, = state["result"]["artifact_refs"]
        payload = n.call("Artifact.Read", {"reference": artifact}, self.token)
        self.assertTrue(payload["authority_id"] and payload["kernel_release"], payload)
        bytes_ = n.call("Artifact.Get", {"reference": artifact})["resource"]["spec"]["size_bytes"]
        return Observation(n.get(task_id)["reference"], state, artifact, payload, int(bytes_))

    def plan_request(self, observations, reason, delay=DELAY):
        return {"task_refs": [item.task for item in observations], "reason": reason, "delay_seconds": delay,
                "max_content_bytes": sum(item.content_bytes for item in observations)}

    def schedule(self, plan):
        return self.node.call(SCHEDULE, {"plan_token": plan["plan_token"]})

    def due(self, record):
        body = record["schedule"]
        self.assertEqual(body["due_boot_ms"] - body["admitted_boot_ms"], 1000 * body["delay_seconds"], record)
        return body["due_boot_ms"] / 1000

    def get(self, record, token=None):
        return self.node.call(GET, {"id": record["reference"]["resource_id"]}, token)

    def wait_schedule(self, record, lifecycle):
        deadline = boottime() + DELAY + 12
        while True:
            current = self.get(record)
            if current["schedule"]["lifecycle"] != "Scheduled" or boottime() >= deadline:
                self.assertEqual(current["schedule"]["lifecycle"], lifecycle, current)
                return current
            time.sleep(0.035)

    def available(self, *observations):
        n = self.node
        for item in observations:
            self.assertEqual(n.get(item.task["resource_id"])["reference"], item.task)
            self.assertEqual(n.call("Task.Get", {"id": item.task["resource_id"]}), item.task_state)
            self.assertEqual(n.call("Artifact.Read", {"reference": item.artifact}, self.token), item.payload)
            retained = n.call("Artifact.Get", {"reference": item.artifact})
            self.assertEqual(retained["availability"], "Available", retained)
            self.assertIsNone(retained["retention"], retained)

    def evicted(self, batch_ref, reason, *observations, schedule_ref=None):
        n = self.node
        batch = n.call("Resource.Resolve", {"reference": batch_ref})["value"]
        self.assertEqual(batch["reason"], reason, batch)
        self.assertEqual(batch.get("schedule_ref"), schedule_ref, batch)
        self.assertEqual(batch["content_bytes_removed"], str(sum(item.content_bytes for item in observations)))
        per_task = {}
        for ref in batch["eviction_refs"]:
            record = n.call("Resource.Resolve", {"reference": ref})["value"]
            per_task[record["previous_task_ref"]["resource_id"]] = (ref, record)
        self.assertEqual(len(batch["eviction_refs"]), len(per_task))
        self.assertEqual(set(per_task), {item.task["resource_id"] for item in observations})
        for item in observations:
            ref, record = per_task[item.task["resource_id"]]
            self.assertEqual(record["previous_task_ref"], item.task, record)
            self.assertEqual(record["artifact_refs"], [item.artifact], record)
            self.assertEqual(int(record["content_bytes_removed"]), item.content_bytes, record)
            self.assertEqual(n.get(item.task["resource_id"])["reference"], record["task_ref"])
            retained = n.call("Artifact.Get", {"reference": item.artifact})
            self.assertEqual(retained["availability"], "Evicted", retained)
            self.assertEqual(retained["retention"], {"reference": ref, "eviction": record}, retained)
            self.assertEqual(n.call("Resource.Resolve", {"reference": item.task})["value"], item.task_state)
            self.denied("ResultEvicted", "Artifact.Read", {"reference": item.artifact}, self.token)

    def create_evidence_hold(self):
        n = self.node
        project = n.call("Project.Create", {"key": key(), "title": "Keep results until verification"})["reference"]
        project_id = project["resource_id"]
        self.grant("local-owner", ["project.read", "project.manage"], [project_id])
        project = n.call("Project.SetMember", {
            **target(project), "principal_id": self.contributor["principal_id"], "role": "Contributor",
        })["reference"]
        self.grant(self.contributor["principal_id"], ["project.read", "project.contribute"], [project_id])
        return n.call("WorkItem.Create", {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Verify one actual result",
            "assignee_principal_id": self.contributor["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
        })["reference"]

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertIs(report["migration_required"], False, report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"], report["witness"]),
                         ("Verified", "Verified", "Confirmed"), report)

    def assert_retention_history(self, *, schedules, batches, evictions):
        with closing(sqlite3.connect(self.node.state / "state.db")) as db:
            rows = db.execute("""
                SELECT kind,count(DISTINCT id) FROM versions
                WHERE kind IN ('TaskResultEvictionSchedule','TaskResultEvictionBatch','TaskResultEviction')
                GROUP BY kind
            """).fetchall()
        actual = dict(rows)
        self.assertEqual(actual.get("TaskResultEvictionSchedule", 0), schedules, actual)
        self.assertEqual(actual.get("TaskResultEvictionBatch", 0), batches, actual)
        self.assertEqual(actual.get("TaskResultEviction", 0), evictions, actual)

    def test_delayed_plan_waits_and_settles_once_after_same_boot_process_loss(self):
        n = self.node
        first, second, outside = self.observe(), self.observe(), self.observe()
        reason = "reclaim only the two explicitly selected artifacts"
        request = self.plan_request((second, first), reason)
        self.denied("PermissionDenied", PREPARE, request, self.token)
        for invalid_delay in (0, 86401, -1, True):
            self.denied("InvalidArgument", PREPARE, {**request, "delay_seconds": invalid_delay})

        request_file = n.base / "delayed-eviction-request.json"
        request_file.write_text(json.dumps(request))
        owner_token = n.witness / "owner.token"
        plan = n.cli(owner_token, "task-result-eviction-plan", request_file)
        self.assertEqual(plan["content_bytes"], first.content_bytes + second.content_bytes, plan)
        self.assertEqual(plan["delay_seconds"], DELAY, plan)
        self.assertCountEqual(plan["items"], [
            {"task_ref": item.task, "content_bytes": item.content_bytes} for item in (first, second)
        ])
        token = plan["plan_token"]
        self.denied("PermissionDenied", SCHEDULE, {"plan_token": token}, self.token)
        changed = ("A" if token[0] != "A" else "B") + token[1:]
        self.denied("InvalidArgument", SCHEDULE, {"plan_token": changed})
        self.denied("PreconditionFailed", APPLY, {"plan_token": token})
        immediate = n.call(PREPARE, {k: v for k, v in request.items() if k != "delay_seconds"})
        self.denied("InvalidArgument", SCHEDULE, {"plan_token": immediate["plan_token"]})
        self.available(first, second, outside)

        with ThreadPoolExecutor(max_workers=3) as callers:
            scheduled = list(callers.map(lambda _: self.schedule(plan), range(3)))
        initial = scheduled[0]
        self.assertEqual(scheduled, [initial] * 3)
        self.assertEqual(initial["schedule"]["lifecycle"], "Scheduled", initial)
        identity = {"id": initial["reference"]["resource_id"]}
        self.denied("PermissionDenied", GET, identity, self.token)
        self.denied("PermissionDenied", CANCEL, target(initial["reference"]), self.token)
        self.denied("PermissionDenied", "Resource.Get", identity)
        self.assertEqual(n.cli(owner_token, "task-result-eviction-schedule-get", identity["id"]), initial)
        due = self.due(initial)
        self.available(first, second, outside)
        self.assertLess(boottime(), due, initial)

        n.stop(signal.SIGKILL)
        after(due)
        n.start()
        settled = self.wait_schedule(initial, "Evicted")
        self.assertNotEqual(settled["reference"], initial["reference"])
        body = settled["schedule"]
        self.assertEqual(body["scheduled_ref"], initial["reference"], body)
        self.assertGreaterEqual(body["executed_boot_ms"], body["due_boot_ms"], body)
        self.evicted(body["batch_ref"], reason, first, second, schedule_ref=initial["reference"])
        self.available(outside)
        self.assertEqual(self.schedule(plan), settled)
        approved_file = n.base / "delayed-eviction-plan.json"
        approved_file.write_text(json.dumps(plan))
        self.assertEqual(n.cli(owner_token, "task-result-eviction-schedule", approved_file), settled)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.schedule(plan), settled)
        self.assertEqual(self.get(initial), settled)
        self.evicted(body["batch_ref"], reason, first, second, schedule_ref=initial["reference"])
        self.verify()
        self.assert_retention_history(schedules=1, batches=1, evictions=2)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            changed = db.execute("""
                UPDATE versions SET body=json_set(body, '$.batch_ref.version', 'r99')
                WHERE id=? AND kind='TaskResultEvictionSchedule' AND rev=2
            """, (identity["id"],))
            self.assertEqual(changed.rowcount, 1)
        corrupted = subprocess.run([*n.daemon, "verify"], text=True, capture_output=True, timeout=20)
        self.assertNotEqual(corrupted.returncode, 0, corrupted.stdout)
        self.assertIn("RecoveryRequired", corrupted.stderr, corrupted.stderr)

    def test_concurrent_cancel_and_authority_change_preserve_all_results(self):
        n = self.node
        first, second, stale = self.observe(), self.observe(), self.observe()
        extra_grant = self.grant(self.contributor["principal_id"], ["task.read"], [self.profile["resource_id"]])
        stale_plan = n.call(PREPARE, self.plan_request((stale,), "scope will change at runtime"))
        scoped = self.schedule(stale_plan)
        self.assertEqual(scoped["schedule"]["lifecycle"], "Scheduled", scoped)
        plan = n.call(PREPARE, self.plan_request((first, second), "cancel before retirement"))
        initial = self.schedule(plan)
        self.assertEqual(initial["schedule"]["lifecycle"], "Scheduled", initial)
        due = max(self.due(initial), self.due(scoped))
        exact = target(initial["reference"])
        self.denied("Conflict", CANCEL, {**exact, "expected_resource_version": "r99"})
        start = Barrier(2)

        def cancel(_):
            start.wait(timeout=10)
            try:
                return n.call(CANCEL, exact)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(max_workers=2) as callers:
            results = list(callers.map(cancel, range(2)))
        self.assertEqual([result for result in results if isinstance(result, str)], ["Conflict"], results)
        cancelled, = [result for result in results if isinstance(result, dict)]
        self.assertEqual(cancelled["schedule"]["lifecycle"], "Cancelled", cancelled)
        self.assertIsNone(cancelled["schedule"].get("batch_ref"), cancelled)
        self.denied("PreconditionFailed", CANCEL, target(cancelled["reference"]))
        self.assertEqual(self.schedule(plan), cancelled)
        n.call("Grant.Revoke", target(extra_grant["grant_ref"]))
        n.stop(signal.SIGKILL)
        after(due)
        n.start()
        self.assertEqual(self.get(initial), cancelled)
        self.assertEqual(self.schedule(plan), cancelled)
        suspended = self.wait_schedule(scoped, "Suspended")
        self.assertEqual(suspended["schedule"]["code"], "ScopeChanged", suspended)
        self.assertIsNone(suspended["schedule"].get("batch_ref"), suspended)
        self.assertEqual(self.schedule(stale_plan), suspended)
        self.available(first, second, stale)
        self.verify()
        self.assert_retention_history(schedules=2, batches=0, evictions=0)

    def test_new_project_evidence_hold_fences_entire_delayed_batch(self):
        n = self.node
        first, held = sorted((self.observe(), self.observe()), key=lambda item: item.task["resource_id"])
        item = self.create_evidence_hold()
        reason = "a late evidence hold takes precedence over delayed retirement"
        plan = n.call(PREPARE, self.plan_request((first, held), reason))
        scheduled = self.schedule(plan)
        self.assertEqual(scheduled["schedule"]["lifecycle"], "Scheduled", scheduled)
        pinned = n.call("WorkItem.SubmitEvidence", {
            **target(item), "task_ref": held.task, "artifact_ref": held.artifact,
        }, self.token)
        self.assertEqual(pinned["value"]["lifecycle"], "InReview", pinned)
        self.available(first, held)

        rejected = self.wait_schedule(scheduled, "Rejected")
        self.assertEqual(rejected["schedule"]["code"], "Conflict", rejected)
        self.assertIsNone(rejected["schedule"].get("batch_ref"), rejected)
        self.available(first, held)
        abandoned = n.call("WorkItem.Abandon", {**target(pinned["reference"]), "reason": "review is no longer needed"})
        self.assertEqual(abandoned["value"]["lifecycle"], "Abandoned", abandoned)
        self.assertEqual(self.schedule(plan), rejected)
        self.available(first, held)

        # A new ordinary approval is required after the rejected schedule.
        replacement = self.plan_request((first, held), reason)
        del replacement["delay_seconds"]
        manual = n.call(PREPARE, replacement)
        receipt = n.call(APPLY, {"plan_token": manual["plan_token"]})
        self.evicted(receipt["reference"], reason, first, held)
        self.assertEqual(self.get(scheduled), rejected)
        self.verify()
        self.assert_retention_history(schedules=1, batches=1, evictions=2)

    def test_storage_failure_rolls_back_earlier_deletion_and_due_work_retries(self):
        n = self.node
        first, second, independent = self.observe(), self.observe(), self.observe()
        ids = (first.artifact["resource_id"], second.artifact["resource_id"])
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            known_ids = [db.execute("SELECT quote(?)", (value,)).fetchone()[0] for value in ids]
            scoped = ",".join(known_ids)
            # Raise at the last artifact only after an earlier deletion occurred
            # in the same transaction. No production-only fault seam is needed.
            db.execute(f"""
                CREATE TRIGGER reject_delayed_final_delete BEFORE DELETE ON artifact_chunks
                WHEN OLD.artifact_id IN ({scoped}) AND NOT EXISTS(
                    SELECT 1 FROM artifact_chunks
                    WHERE artifact_id IN ({scoped}) AND artifact_id<>OLD.artifact_id
                ) BEGIN SELECT RAISE(ABORT, 'fixture interrupted delayed deletion'); END
            """)
        daemon = n.daemon
        diagnostic = n.base / "delayed-worker.log"
        n.daemon = ["sh", "-c", 'log=$1; shift; exec "$@" 2>>"$log"', "psios-test", str(diagnostic), *daemon]
        n.start()
        reason = "retry a due batch without leaking partially deleted results"
        plan = n.call(PREPARE, self.plan_request((first, second), reason, delay=1))
        schedule = self.schedule(plan)
        other_reason = "unrelated due work can settle during a scoped fault"
        other = self.schedule(n.call(PREPARE, self.plan_request((independent,), other_reason, delay=2)))
        deadline = self.due(schedule) + 12
        while "scheduled result eviction: StorageUnavailable" not in diagnostic.read_text():
            self.assertIsNone(n.process.poll(), diagnostic.read_text())
            self.assertLess(boottime(), deadline, diagnostic.read_text())
            time.sleep(0.05)
        self.assertEqual(self.get(schedule), schedule)
        self.available(first, second)
        other_done = self.wait_schedule(other, "Evicted")
        self.evicted(other_done["schedule"]["batch_ref"], other_reason, independent,
                     schedule_ref=other["reference"])
        self.assertEqual(self.get(schedule), schedule)
        self.available(first, second)

        n.stop(signal.SIGKILL)
        self.assert_retention_history(schedules=2, batches=1, evictions=1)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            self.assertEqual(db.execute(
                "SELECT count(DISTINCT artifact_id) FROM artifact_chunks WHERE artifact_id IN (?,?)", ids,
            ).fetchone(), (2,))
            db.execute("DROP TRIGGER reject_delayed_final_delete")
        n.daemon = daemon
        n.start()
        settled = self.wait_schedule(schedule, "Evicted")
        self.evicted(settled["schedule"]["batch_ref"], reason, first, second, schedule_ref=schedule["reference"])
        self.verify()
        self.assert_retention_history(schedules=2, batches=2, evictions=3)

    def test_authority_reserve_rejects_new_schedule_and_keeps_recovery_available(self):
        n = self.node
        due, cancelled, unopened = (self.observe() for _ in range(3))
        due_reason = "settle an existing schedule under its filesystem reserve"
        due_plan = n.call(PREPARE, self.plan_request((due,), due_reason))
        due_schedule = self.schedule(due_plan)
        postponed = n.call(PREPARE, self.plan_request(
            (cancelled,), "cancel under the configured reserve", delay=60,
        ))
        cancellable = self.schedule(postponed)
        fresh = n.call(PREPARE, self.plan_request(
            (unopened,), "deny any additional schedule admission under the reserve",
        ))

        # Set the fixture's admission floor above its actual filesystem size.
        # This exercises statvfs without allocating blocks on the development host.
        n.stop(signal.SIGKILL)
        config = json.loads(n.config.read_text())
        space = os.statvfs(n.state)
        config["authority_reserve"] = {"state_bytes": space.f_blocks * space.f_frsize + 1}
        n.config.write_text(json.dumps(config))
        n.start()
        self.assertTrue(n.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        self.denied("QuotaExceeded", SCHEDULE, {"plan_token": fresh["plan_token"]})
        self.assertEqual(self.get(cancellable), self.schedule(postponed))
        stopped = n.call(CANCEL, target(cancellable["reference"]))
        self.assertEqual(stopped["schedule"]["lifecycle"], "Cancelled")
        self.assertEqual(self.schedule(postponed), stopped)

        done = self.wait_schedule(due_schedule, "Evicted")
        self.assertEqual(self.schedule(due_plan), done)
        self.evicted(done["schedule"]["batch_ref"], due_reason, due,
                     schedule_ref=due_schedule["reference"])
        self.available(cancelled, unopened)
        self.verify()
        self.assert_retention_history(schedules=2, batches=1, evictions=1)


if __name__ == "__main__":
    unittest.main()
