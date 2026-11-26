"""Verify scoped reassignment and cleanup of abandoned WorkItems over Unix RPC."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
import unittest

from support import Failure, Node, key, target


class WorkItemMaintenanceTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.original = self.node.call("Agent.Register", {"name": "original-contributor"})
        self.replacement = self.node.call("Agent.Register", {"name": "replacement-contributor"})
        self.reviewer = self.node.call("Agent.Register", {"name": "independent-reviewer"})
        project = self.node.call("Project.Create", {"key": key(), "title": "Host observation handoff"})
        self.project_id = project["reference"]["resource_id"]
        self.profile = self.node.get("profile-local")["reference"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [self.project_id])
        self.member(self.original, "Contributor")
        self.member(self.replacement, "Contributor")
        self.member(self.reviewer, "Reviewer")
        self.grant(self.original, ["project.read", "project.contribute"], [self.project_id])
        self.grant(self.replacement, ["project.read"], [self.project_id])
        self.grant(self.reviewer, ["project.read", "project.manage", "project.review"], [self.project_id])

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor is not None else None)

    def grant(self, actor, actions, targets):
        return self.node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions, "tool_targets": targets,
            "units": 4, "ttl_seconds": 240,
        })

    def member(self, actor, role):
        project = self.node.call("Project.Get", {"id": self.project_id})
        self.node.call("Project.SetMember", {
            **target(project["reference"]), "principal_id": actor["principal_id"], "role": role,
        })

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.call(method, params, actor)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def create(self, assignee, title, blocked_by=()):
        project = self.node.call("Project.Get", {"id": self.project_id})
        request = {
            "project_id": self.project_id, "expected_project_version": project["reference"]["version"],
            "key": key(), "title": title, "assignee_principal_id": assignee["principal_id"],
            "blocked_by": list(blocked_by),
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
        }
        return request, self.node.call("WorkItem.Create", request)

    def task(self, actor):
        grant = self.grant(actor, ["task.submit", "task.read", "system.status"], [self.profile["resource_id"]])
        request = self.node.prepare("system.status", {}, token=actor["token"],
                                    delegation=grant["delegation_ref"]["resource_id"])
        task_id = self.call("Task.Submit", request, actor)["task_id"]
        task = self.node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        return self.node.get(task_id)["reference"], artifact

    def evidence(self, item, task, artifact):
        return {**target(item["reference"]), "task_ref": task, "artifact_ref": artifact}

    def get_item(self, item, actor=None):
        return self.call("WorkItem.Get", {"id": item["reference"]["resource_id"]}, actor)

    def evict_request(self, task):
        return {**target(task), "reason": "release project observation bytes"}

    def race(self, method, requests):
        gate = Barrier(2)

        def mutate(request):
            gate.wait(timeout=5)
            try:
                return self.node.call(method, request)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(max_workers=2) as workers:
            outcomes = list(workers.map(mutate, requests))
        self.assertEqual(outcomes.count("Conflict"), 1, outcomes)
        winner, = [outcome for outcome in outcomes if isinstance(outcome, dict)]
        return winner

    def audited_once(self, method, item):
        audit = self.node.call("Audit.List", {"limit": 128})
        events = [entry for entry in audit if entry["method"] == method
                  and entry["resource"] == item["reference"]["resource_id"] and entry["decision"] == "Allowed"]
        self.assertEqual([entry["actor"] for entry in events], ["local-owner"], events)

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 81, report)
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_reassign_disabled_contributor_then_review_new_task(self):
        n = self.node
        creation, item = self.create(self.original, "Replace a disabled assignee")
        old_task, old_artifact = self.task(self.original)
        past_review = self.call("WorkItem.SubmitEvidence", self.evidence(item, old_task, old_artifact), self.original)
        self.grant(self.reviewer, ["task.results.read"], [old_task["resource_id"]])
        item = self.call("WorkItem.Review", {**target(past_review["reference"]), "accept": False}, self.reviewer)
        self.assertEqual(item["value"]["lifecycle"], "Ready", item)
        self.assertFalse(item["value"]["has_evidence"], item)
        old_principal = n.get(self.original["principal_id"])["reference"]
        n.call("Principal.Revoke", target(old_principal))
        self.denied("AuthenticationRequired", "WorkItem.SubmitEvidence", self.evidence(item, old_task, old_artifact),
                    self.original)

        request = {**target(item["reference"]), "assignee_principal_id": self.replacement["principal_id"]}
        self.denied("PermissionDenied", "WorkItem.Reassign", request)
        self.grant(self.replacement, ["project.read", "project.contribute"], [self.project_id])
        self.denied("PermissionDenied", "WorkItem.Reassign", request, self.reviewer)
        assigned = self.race("WorkItem.Reassign", (request, request))
        self.assertEqual(assigned["value"]["lifecycle"], "Ready", assigned)
        self.assertEqual(assigned["value"]["assignee_principal_id"], self.replacement["principal_id"], assigned)
        self.assertIsNone(assigned["value"]["review"], assigned)
        change = assigned["value"]["assignment"]
        self.assertEqual(change["previous_assignee_principal_id"], self.original["principal_id"], change)
        self.assertEqual(change["assigned_by_principal_id"], "local-owner", change)
        self.assertTrue(change["assigned_at"], change)
        self.assertEqual(n.call("WorkItem.Create", creation), assigned)
        self.denied("IdempotencyConflict", "WorkItem.Create", {
            **creation, "assignee_principal_id": self.replacement["principal_id"],
        })

        self.restart()
        self.assertEqual(self.get_item(item, self.replacement), assigned)
        self.audited_once("WorkItem.Reassign", assigned)
        self.grant(self.replacement, ["task.results.read"], [old_task["resource_id"]])
        self.assertTrue(self.call("Artifact.Read", {"reference": old_artifact}, self.replacement)["authority_id"])
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", self.evidence(assigned, old_task, old_artifact),
                    self.replacement)
        new_task, new_artifact = self.task(self.replacement)
        pending = self.call("WorkItem.SubmitEvidence", self.evidence(assigned, new_task, new_artifact),
                            self.replacement)
        self.assertEqual(pending["value"]["lifecycle"], "InReview", pending)

        self.member(self.reviewer, "Contributor")
        self.grant(self.reviewer, ["project.read", "project.contribute"], [self.project_id])
        self.denied("PreconditionFailed", "WorkItem.Reassign", {
            **target(pending["reference"]), "assignee_principal_id": self.reviewer["principal_id"],
        })
        self.member(self.reviewer, "Reviewer")
        self.grant(self.reviewer, ["task.results.read"], [new_task["resource_id"]])
        verified = self.call("WorkItem.Review", {**target(pending["reference"]), "accept": True}, self.reviewer)
        self.assertEqual(verified["value"]["lifecycle"], "Verified", verified)
        self.assertEqual(verified["value"]["review"]["reviewer_principal_id"], self.reviewer["principal_id"])
        self.assertEqual(verified["value"]["evidence"]["task_ref"], new_task)
        self.assertEqual(verified["value"]["assignment"], change)
        self.assertEqual(n.get(old_task["resource_id"])["reference"], old_task)

        evict = self.evict_request(new_task)
        self.denied("Conflict", "Task.EvictResults", evict)
        ended = n.call("WorkItem.Abandon", {**target(verified["reference"]), "reason": "verified work superseded"})
        self.assertEqual(ended["value"]["lifecycle"], "Abandoned", ended)
        self.assertEqual(ended["value"]["abandonment"]["prior_lifecycle"], "Verified", ended)
        self.assertEqual(ended["value"]["review"], verified["value"]["review"], ended)
        self.assertIsNone(ended["value"].get("evidence"), ended)
        n.call("Task.EvictResults", evict)
        self.denied("ResultEvicted", "Artifact.Read", {"reference": new_artifact}, self.replacement)
        self.verify()

    def test_abandon_pending_work_releases_pin_without_unblocking_dependents(self):
        n = self.node
        _, item = self.create(self.original, "Cancel this review")
        item_id = item["reference"]["resource_id"]
        _, follow_up = self.create(self.original, "Depends on accepted closure", [item_id])
        _, empty = self.create(self.original, "Withdraw before submission")
        withdrawn = n.call("WorkItem.Abandon", {**target(empty["reference"]), "reason": "scope removed"})
        self.assertEqual(withdrawn["value"]["lifecycle"], "Abandoned", withdrawn)
        self.assertEqual(withdrawn["value"]["abandonment"]["prior_lifecycle"], "Ready", withdrawn)
        self.assertFalse(withdrawn["value"]["has_evidence"], withdrawn)

        task, artifact = self.task(self.original)
        pending = self.call("WorkItem.SubmitEvidence", self.evidence(item, task, artifact), self.original)
        original_operations = n.call("Resource.List", {"kind": "Operation"})["items"]
        self.assertEqual(len(original_operations), 1, original_operations)
        read = {"reference": artifact}
        original_payload = self.call("Artifact.Read", read, self.original)
        self.assertTrue(original_payload["authority_id"], original_payload)
        evict = self.evict_request(task)
        self.denied("Conflict", "Task.EvictResults", evict)
        params = target(pending["reference"])
        self.denied("InvalidArgument", "WorkItem.Abandon", {**params, "reason": "   "})
        self.denied("InvalidArgument", "WorkItem.Abandon", {**params, "reason": "a" * 513})
        self.denied("PermissionDenied", "WorkItem.Abandon", {**params, "reason": "not my decision"}, self.reviewer)
        reasons = ("require fresh baseline", "withdraw stale investigation")
        abandoned = self.race("WorkItem.Abandon", tuple({**params, "reason": reason} for reason in reasons))
        self.assertEqual(abandoned["value"]["lifecycle"], "Abandoned", abandoned)
        self.assertTrue(abandoned["value"]["has_evidence"], abandoned)
        self.assertIsNone(abandoned["value"].get("evidence"), abandoned)
        reason = abandoned["value"]["abandonment"]
        self.assertIn(reason["reason"], reasons, reason)
        self.assertEqual((reason["prior_lifecycle"], reason["abandoned_by_principal_id"]),
                         ("InReview", "local-owner"), reason)
        self.assertTrue(reason["abandoned_at"], reason)

        self.restart()
        recovered = self.get_item(abandoned, self.original)
        self.assertEqual(recovered["reference"], abandoned["reference"], recovered)
        self.assertEqual(recovered["value"]["abandonment"], reason, recovered)
        self.assertEqual(recovered["value"]["evidence"]["artifact_ref"], artifact, recovered)
        self.assertEqual(self.get_item(empty)["reference"], withdrawn["reference"])
        self.audited_once("WorkItem.Abandon", abandoned)
        self.assertEqual(n.get(task["resource_id"])["reference"], task)
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [task])
        self.assertEqual(n.call("Resource.List", {"kind": "Operation"})["items"], original_operations)
        self.assertEqual(self.call("Artifact.Read", read, self.original), original_payload)
        blocked = self.get_item(follow_up, self.original)
        self.assertEqual(blocked["value"]["blockers"], [{"id": item_id, "lifecycle": "Abandoned"}], blocked)
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", self.evidence(follow_up, task, artifact),
                    self.original)

        current = target(recovered["reference"])
        self.grant(self.replacement, ["project.read", "project.contribute"], [self.project_id])
        self.denied("PreconditionFailed", "WorkItem.Reassign", {
            **current, "assignee_principal_id": self.replacement["principal_id"],
        })
        self.denied("PreconditionFailed", "WorkItem.Abandon", {**current, "reason": "rewrite the reason"})
        self.denied("PreconditionFailed", "WorkItem.Close", current)
        self.grant(self.reviewer, ["task.results.read"], [task["resource_id"]])
        self.denied("PreconditionFailed", "WorkItem.Review", {**current, "accept": True}, self.reviewer)
        n.call("Task.EvictResults", evict)
        self.denied("ResultEvicted", "Artifact.Read", read, self.original)
        self.assertEqual(self.get_item(abandoned, self.original), recovered)
        self.verify()


if __name__ == "__main__":
    unittest.main()
