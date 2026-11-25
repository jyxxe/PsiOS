"""Dispatch only after a pinned Task output is available and retained."""
import hashlib
import json
import os
import signal
import sqlite3
import unittest
from datetime import datetime, timedelta, timezone

from support import Failure, Node, key, target


class TaskDependenciesTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "dependency-source"})
        self.data = self.n.state / "volumes" / self.volume["resource_id"] / "data"
        (self.data / "record").write_text("confirmed input")
        canonical = json.dumps({"name": "record", "content": "confirmed input"},
                               sort_keys=True, separators=(",", ":")).encode()
        self.digest = "sha256:" + hashlib.sha256(canonical).hexdigest()

    def scheduling(self, mode):
        state = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {"expected_resource_version": state["version"], "scheduling": mode})

    def source(self):
        return self.n.submit("volume.read", {**target(self.volume), "name": "record"})

    def prepare(self, requirements, *, token=None, delegation="owner-delegation", tool="system.status", data=None):
        return self.n.call("Task.Prepare", {
            "key": key(), "tool": tool, "input": data or {}, "delegation_id": delegation,
            "requires": requirements,
        }, token)

    def task(self, identity):
        return self.n.call("Task.Get", {"id": identity})

    def requirement(self, producer):
        return {"task_id": producer, "digest": self.digest}

    def assert_waiting(self, task):
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("WaitingResource", "NotStarted"), task)
        self.assertNotIn("current_run_id", task)
        self.assertEqual(task["conditions"][0]["type"], "DependencyUnavailable")

    def test_cli_dependency_survives_restart_and_pins_before_effect_approval(self):
        n = self.n
        self.scheduling("Cordoned")
        upstream = self.source()
        manifest = n.base / "dependencies.json"
        manifest.write_text(json.dumps([self.requirement(upstream)]))
        tool_input = n.base / "write.json"
        tool_input.write_text(json.dumps({**target(self.volume), "name": "dependent", "content": "authorized later"}))
        args = ("submit", "volume.write", "--key", "typed-dependencies", "--requires", manifest, tool_input)
        first = n.cli(n.witness / "owner.token", *args)
        replay = n.cli(n.witness / "owner.token", *args)
        dependent = first["task_id"]
        self.assertEqual(replay["task_id"], dependent)
        self.assertTrue(replay["deduplicated"])
        waiting = self.task(dependent)
        self.assert_waiting(waiting)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.task(dependent), waiting)
        self.scheduling("Enabled")
        produced = n.wait(upstream)["result"]["artifact_refs"][0]
        self.assertEqual(produced["digest"], self.digest)
        ready = n.wait(dependent)
        self.assertEqual(ready["lifecycle"], "WaitingApproval")
        self.assertFalse((self.data / "dependent").exists())
        eviction = {**target(n.get(upstream)["reference"]), "reason": "after consumers settle"}
        with self.assertRaises(Failure) as retained:
            n.call("Task.EvictResults", eviction)
        self.assertEqual(retained.exception.code, "Conflict")

        n.approve(dependent)
        completed = n.wait(dependent)
        self.assertEqual(completed["lifecycle"], "Succeeded", completed)
        self.assertEqual((self.data / "dependent").read_text(), "authorized later")
        run = n.call("Run.Get", {"id": completed["current_run_id"]})
        snapshot = n.get(run["execution_snapshot_ref"]["resource_id"])["value"]
        self.assertEqual(snapshot["required_artifact_refs"], [produced])
        n.call("Task.EvictResults", eviction)
        self.assertEqual(self.task(upstream)["result"]["availability"], "Evicted")

    def test_missing_result_waits_until_authoritative_verification(self):
        n = self.n
        upstream = self.source()
        reference = n.wait(upstream)["result"]["artifact_refs"][0]
        chunk = n.call("Artifact.ReadChunk", {"reference": reference, "offset": 0, "length": 1024})
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            self.assertEqual(database.execute(
                "DELETE FROM artifact_chunks WHERE artifact_id=?", (reference["resource_id"],)).rowcount, 1)
        unavailable = n.call("Task.VerifyResults", target(n.get(upstream)["reference"]))
        self.assertEqual(unavailable["result"]["availability"], "Unavailable")
        request = self.prepare([self.requirement(upstream)])
        dependent = n.call("Task.Submit", request)["task_id"]
        self.assert_waiting(self.task(dependent))
        n.call("Artifact.RepairChunk", {
            "reference": reference, "offset": 0, "data_base64": chunk["data_base64"],
        })
        self.assert_waiting(self.task(dependent))
        restored = n.call("Task.VerifyResults", target(n.get(upstream)["reference"]))
        self.assertEqual(restored["result"]["availability"], "Available")
        self.assertEqual(n.wait(dependent)["lifecycle"], "Succeeded")

    def test_approved_effect_does_not_run_when_the_required_result_is_lost(self):
        n = self.n
        upstream = self.source()
        artifact = n.wait(upstream)["result"]["artifact_refs"][0]
        request = self.prepare([self.requirement(upstream)], tool="volume.write",
                               data={**target(self.volume), "name": "fenced", "content": "would write"})
        dependent = n.call("Task.Submit", request)["task_id"]
        self.assertEqual(n.wait(dependent)["lifecycle"], "WaitingApproval")
        self.scheduling("Cordoned")
        n.approve(dependent)
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("DELETE FROM artifact_chunks WHERE artifact_id=?", (artifact["resource_id"],))
        unavailable = n.call("Task.VerifyResults", target(n.get(upstream)["reference"]))
        self.assertEqual(unavailable["result"]["availability"], "Unavailable")
        self.scheduling("Enabled")
        blocked = n.wait(dependent)
        self.assertEqual((blocked["lifecycle"], blocked["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertEqual(blocked["conditions"][0]["type"], "ResultUnavailable")
        self.assertNotIn("current_run_id", blocked)
        self.assertFalse((self.data / "fenced").exists())

    def test_invalid_and_cancelled_dependencies_do_not_consume_execution_units(self):
        n = self.n
        self.scheduling("Cordoned")
        upstream = self.source()
        wrong = {**self.requirement(upstream), "digest": "sha256:" + "0" * 64}
        request = self.prepare([wrong])
        downstream = n.call("Task.Submit", request)["task_id"]
        self.assert_waiting(self.task(downstream))
        budget_id = request["budget_ref"]["resource_id"]
        initial = n.call("Budget.Get", {"id": budget_id})
        cancel = n.call("Task.Submit", self.prepare([self.requirement(upstream)]))["task_id"]
        self.assert_waiting(self.task(cancel))
        n.call("Task.Cancel", target(n.get(cancel)["reference"]))
        self.assertEqual(n.call("Budget.Get", {"id": budget_id}), initial)

        actor, grant = n.agent(["task.submit", "task.read", "system.status"])
        with self.assertRaises(Failure) as denied:
            self.prepare([self.requirement(upstream)], token=actor["token"],
                         delegation=grant["delegation_ref"]["resource_id"])
        self.assertEqual(denied.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as duplicate:
            self.prepare([wrong, self.requirement(upstream)])
        self.assertEqual(duplicate.exception.code, "InvalidArgument")
        self.scheduling("Enabled")
        self.assertEqual(n.wait(upstream)["lifecycle"], "Succeeded")
        failed = n.wait(downstream)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "NotStarted"), failed)
        self.assertEqual(failed["conditions"][0]["type"], "PreconditionFailed")
        self.assertNotIn("current_run_id", failed)
        final = n.call("Budget.Get", {"id": budget_id})
        self.assertEqual((final["reserved"], final["spent"]), (initial["reserved"] - 2, initial["spent"] + 1))

    def test_dependency_admission_rolls_back_and_waiting_task_still_expires(self):
        n = self.n
        self.scheduling("Cordoned")
        upstream = self.source()
        deadline = (datetime.now(timezone.utc) + timedelta(seconds=5)).isoformat().replace("+00:00", "Z")
        request = n.call("Task.Prepare", {
            "key": key(), "tool": "system.status", "input": {}, "delegation_id": "owner-delegation",
            "requires": [self.requirement(upstream)], "deadline_at": deadline,
        })
        budget_id = request["budget_ref"]["resource_id"]
        budget = n.call("Budget.Get", {"id": budget_id})
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("""CREATE TRIGGER reject_dependency BEFORE INSERT ON task_dependencies
                BEGIN SELECT RAISE(ABORT, 'injected dependency pin failure'); END""")
        with self.assertRaises(Failure) as rolled_back:
            n.call("Task.Submit", request)
        self.assertEqual(rolled_back.exception.code, "StorageUnavailable")
        self.assertEqual(n.call("Task.LookupSubmission", request)["status"], "AbsentWithinWindow")
        self.assertEqual(n.call("Budget.Get", {"id": budget_id}), budget)
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER reject_dependency")
        dependent = n.call("Task.Submit", request)["task_id"]
        self.assert_waiting(self.task(dependent))
        expired = n.wait(dependent)
        self.assertEqual((expired["lifecycle"], expired["outcome_knowledge"]), ("Failed", "NotStarted"), expired)
        self.assertEqual(expired["conditions"][0]["type"], "DeadlineExceeded")
        self.assertNotIn("current_run_id", expired)
        self.assertEqual(n.call("Budget.Get", {"id": budget_id}), budget)


if __name__ == "__main__":
    unittest.main()
