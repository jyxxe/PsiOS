"""Exercise signed multi-Task result eviction and all-or-nothing SQLite settlement."""
from contextlib import closing
from dataclasses import dataclass
from datetime import datetime, timezone
import json
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target


PREPARE = "Task.PrepareResultEvictionBatch"
APPLY = "Task.ApplyResultEvictionBatch"


@dataclass(frozen=True)
class Observation:
    task: dict
    task_state: dict
    artifact: dict
    payload: dict
    content_bytes: int


class TaskResultEvictionBatchTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.contributor = self.node.call("Agent.Register", {"name": "batch-result-contributor"})
        self.token = self.contributor["token"]
        self.profile = self.node.get("profile-local")["reference"]
        self.execution = self.grant(self.contributor["principal_id"],
                                    ["task.submit", "task.read", "resource.read", "system.status"],
                                    [self.profile["resource_id"]])["delegation_ref"]["resource_id"]

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
        info = n.call("Artifact.Get", {"reference": artifact})
        return Observation(n.get(task_id)["reference"], state, artifact, payload,
                           int(info["resource"]["spec"]["size_bytes"]))

    def prepare(self, observations, reason, **options):
        request = {"task_refs": [item.task for item in observations], "reason": reason, **options}
        return self.node.call(PREPARE, request)

    def apply(self, plan):
        return self.node.call(APPLY, {"plan_token": plan["plan_token"]})

    def assert_available(self, *observations):
        for item in observations:
            n = self.node
            self.assertEqual(n.get(item.task["resource_id"])["reference"], item.task)
            self.assertEqual(n.call("Task.Get", {"id": item.task["resource_id"]}), item.task_state)
            self.assertEqual(n.call("Artifact.Read", {"reference": item.artifact}, self.token), item.payload)
            info = n.call("Artifact.Get", {"reference": item.artifact})
            self.assertEqual(info["availability"], "Available", info)
            self.assertIsNone(info["retention"], info)

    def assert_evicted(self, receipt, reason, *observations):
        n = self.node
        body = receipt["batch"]
        self.assertEqual(body["reason"], reason, body)
        self.assertEqual(body["content_bytes_removed"], str(sum(item.content_bytes for item in observations)))
        self.assertEqual(len(body["eviction_refs"]), len(observations), body)
        per_task = {}
        for ref in body["eviction_refs"]:
            record = n.call("Resource.Resolve", {"reference": ref})["value"]
            per_task[record["previous_task_ref"]["resource_id"]] = (ref, record)
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
            self.denied("ResultEvicted", "Artifact.ReadChunk", {
                "reference": item.artifact, "offset": 0, "length": 1,
            }, self.token)

    def work_item(self, pin_candidate):
        n = self.node
        reviewer = n.call("Agent.Register", {"name": "batch-retention-reviewer"})
        project = n.call("Project.Create", {"key": key(), "title": "Independently reviewed retention"})["reference"]
        project_id = project["resource_id"]
        self.grant("local-owner", ["project.read", "project.manage"], [project_id])
        for agent, role, action in ((self.contributor, "Contributor", "project.contribute"),
                                    (reviewer, "Reviewer", "project.review")):
            project = n.call("Project.SetMember", {
                **target(project), "principal_id": agent["principal_id"], "role": role,
            })["reference"]
            self.grant(agent["principal_id"], ["project.read", action], [project_id])
        self.grant(reviewer["principal_id"], ["task.results.read"], [pin_candidate.task["resource_id"]])
        item = n.call("WorkItem.Create", {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Check the actual observation",
            "assignee_principal_id": self.contributor["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
        })["reference"]
        return item, reviewer["token"]

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_fresh_pin_denies_entire_batch_then_same_plan_applies_and_replays(self):
        n = self.node
        first, second = sorted([self.observe(), self.observe()], key=lambda item: item.task["resource_id"])
        stale = self.observe()
        item, reviewer_token = self.work_item(second)
        reason = "retire selected observation results"
        request = {"task_refs": [second.task, first.task], "reason": reason}
        self.denied("PermissionDenied", PREPARE, request, self.token)
        self.denied("InvalidArgument", PREPARE, {**request, "task_refs": [first.task, first.task]})
        self.denied("PreconditionFailed", PREPARE, {
            **request, "task_refs": [first.task, {**second.task, "digest": "sha256:" + "0" * 64}],
        })
        size = first.content_bytes + second.content_bytes
        self.denied("QuotaExceeded", PREPARE, {**request, "max_content_bytes": size - 1})

        stale_plan = self.prepare([first, stale], "the second task revision will change")
        n.call("Task.EvictResults", {**target(stale.task), "reason": "explicitly evict one result first"})
        self.denied("Conflict", APPLY, {"plan_token": stale_plan["plan_token"]})
        self.assert_available(first, second)
        plan = n.call(PREPARE, {**request, "max_content_bytes": size})
        self.assertEqual(plan["content_bytes"], size, plan)
        self.assertCountEqual([(entry["task_ref"], entry["content_bytes"]) for entry in plan["items"]],
                              [(first.task, first.content_bytes), (second.task, second.content_bytes)], plan)
        valid_until = datetime.fromisoformat(plan["valid_until"].replace("Z", "+00:00"))
        self.assertGreater(valid_until, datetime.now(timezone.utc))
        token = plan["plan_token"]
        self.assertIsInstance(token, str)
        self.assertTrue(token)
        self.denied("PermissionDenied", APPLY, {"plan_token": token}, self.token)
        middle = len(token) // 2
        tampered = token[:middle] + ("A" if token[middle] != "A" else "B") + token[middle + 1:]
        self.denied("InvalidArgument", APPLY, {"plan_token": tampered})
        self.assert_available(first, second)

        pinned = n.call("WorkItem.SubmitEvidence", {
            **target(item), "task_ref": second.task, "artifact_ref": second.artifact,
        }, self.token)
        self.assertEqual(pinned["value"]["lifecycle"], "InReview", pinned)
        self.denied("Conflict", APPLY, {"plan_token": token})
        self.assert_available(first, second)
        allowed = n.call("WorkItem.Review", {**target(pinned["reference"]), "accept": True}, reviewer_token)
        self.denied("Conflict", APPLY, {"plan_token": token})
        self.assert_available(first, second)
        closed = n.call("WorkItem.Close", target(allowed["reference"]))
        self.assertEqual(closed["value"]["lifecycle"], "Closed", closed)
        self.assert_available(first, second)

        settled = self.apply(plan)
        self.assert_evicted(settled, reason, first, second)
        self.grant(self.contributor["principal_id"], ["task.read"], [self.profile["resource_id"]])
        self.restart()
        self.assertEqual(self.apply(plan), settled)
        self.assert_evicted(settled, reason, first, second)
        audit = n.call("Audit.List", {"limit": 128})
        batch_audit = [entry for entry in audit if entry["method"] == APPLY and entry["decision"] == "Evicted"]
        self.assertEqual([entry["resource"] for entry in batch_audit], [settled["reference"]["resource_id"]])
        self.verify()

    def test_final_chunk_failure_rolls_back_all_prior_chunk_deletes(self):
        n = self.node
        first, second = self.observe(), self.observe()
        artifact_ids = (first.artifact["resource_id"], second.artifact["resource_id"])
        task_ids = (first.task["resource_id"], second.task["resource_id"])
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            self.assertEqual(db.execute(
                "SELECT count(DISTINCT artifact_id) FROM artifact_chunks WHERE artifact_id IN (?,?)", artifact_ids,
            ).fetchone(), (2,))
            left, right = (db.execute("SELECT quote(?)", (value,)).fetchone()[0] for value in artifact_ids)
            # Fire only after the other selected artifact's chunks have already been deleted in this transaction.
            db.execute(f"""
                CREATE TRIGGER reject_final_batch_chunk BEFORE DELETE ON artifact_chunks
                WHEN OLD.artifact_id IN ({left},{right}) AND NOT EXISTS(
                    SELECT 1 FROM artifact_chunks
                    WHERE artifact_id IN ({left},{right}) AND artifact_id<>OLD.artifact_id
                ) BEGIN SELECT RAISE(ABORT, 'fixture lost the final artifact deletion'); END
            """)
        n.start()
        reason = "test a partial physical deletion"
        plan = self.prepare([first, second], reason)
        self.denied("StorageUnavailable", APPLY, {"plan_token": plan["plan_token"]})
        self.assert_available(first, second)
        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            self.assertEqual(db.execute(
                "SELECT count(DISTINCT artifact_id) FROM artifact_chunks WHERE artifact_id IN (?,?)", artifact_ids,
            ).fetchone(), (2,))
            self.assertEqual(db.execute("""
                SELECT count(*) FROM versions
                WHERE kind='TaskResultEviction' AND spec->>'task_id' IN (?,?)
            """, task_ids).fetchone(), (0,))
            db.execute("DROP TRIGGER reject_final_batch_chunk")
        n.start()
        self.assert_available(first, second)
        receipt = self.apply(self.prepare([first, second], reason))
        self.assert_evicted(receipt, reason, first, second)
        self.verify()


if __name__ == "__main__":
    unittest.main()
