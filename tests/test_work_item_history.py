"""Check immutable private WorkItem history through Core's Unix API."""
import json
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key, target


class WorkItemHistoryTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.assignee = self.register("history-contributor")
        self.replacement = self.register("history-replacement")
        self.reviewer = self.register("history-reviewer")
        self.outsider = self.register("history-nonmember")
        self.project = self.call("Project.Create", {"key": key(), "title": "Historical host observations"})
        self.project_id = self.project["reference"]["resource_id"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [self.project_id])
        for actor, role, action in (
            (self.assignee, "Contributor", "project.contribute"),
            (self.replacement, "Contributor", "project.contribute"),
            (self.reviewer, "Reviewer", "project.review"),
        ):
            self.member(actor, role)
            self.grant(actor, ["project.read", action], [self.project_id])
        self.grant(self.outsider, ["project.read"], [self.project_id])
        self.profile = self.node.get("profile-local")["reference"]

    def register(self, name):
        return self.call("Agent.Register", {"name": name})

    def call(self, rpc, params, actor=None):
        return self.node.call(rpc, params, actor["token"] if actor else None)

    def denied(self, code, rpc, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.call(rpc, params, actor)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def grant(self, actor, actions, targets):
        return self.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions,
            "tool_targets": targets, "units": 4, "ttl_seconds": 300,
        })

    def member(self, actor, role):
        self.project = self.call("Project.SetMember", {
            **target(self.project["reference"]), "principal_id": actor["principal_id"], "role": role,
        })

    def create(self, title, blocked_by=()):
        return self.call("WorkItem.Create", {
            "project_id": self.project_id, "expected_project_version": self.project["reference"]["version"],
            "key": key(), "title": title, "assignee_principal_id": self.assignee["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
            "blocked_by": list(blocked_by),
        })["reference"]

    def observe(self, actor):
        grant = self.grant(actor, ["task.submit", "task.read", "system.status"], [self.profile["resource_id"]])
        request = self.node.prepare("system.status", {}, token=actor["token"],
                                    delegation=grant["delegation_ref"]["resource_id"])
        task_id = self.call("Task.Submit", request, actor)["task_id"]
        result = self.node.wait(task_id)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Succeeded", "Known"), result)
        artifact, = result["result"]["artifact_refs"]
        payload = self.call("Artifact.Read", {"reference": artifact}, actor)
        self.assertTrue(payload["kernel_release"] and payload["authority_id"], payload)
        return self.node.get(task_id)["reference"], artifact, payload

    def submit(self, item, task, artifact, actor):
        return self.call("WorkItem.SubmitEvidence", {**target(item), "task_ref": task, "artifact_ref": artifact},
                         actor)["reference"]

    def review(self, item, accept):
        return self.call("WorkItem.Review", {**target(item), "accept": accept}, self.reviewer)["reference"]

    def share_result(self, task):
        return self.grant(self.reviewer, ["task.results.read"], [task["resource_id"]])

    def history(self, item, actor=None, **params):
        return self.call("WorkItem.History", {"id": item["resource_id"], **params}, actor)

    def revision(self, reference, actor=None):
        revision = self.call("WorkItem.RevisionGet", {"reference": reference}, actor)
        self.assertEqual(revision["reference"], reference, revision)
        return revision["value"]

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def assert_metadata_only(self, view, task, artifact):
        encoded = json.dumps(view)
        self.assertNotIn(task["resource_id"], encoded)
        self.assertNotIn(artifact["resource_id"], encoded)
        self.assertNotIn("kernel_release", encoded)
        self.assertNotIn("uptime_seconds", encoded)

    def test_fixed_snapshot_across_reassignment_closure_and_crash(self):
        n = self.node
        created = self.create("Close the accepted observation")
        dependent = self.create("Keep the recorded prerequisite", [created["resource_id"]])
        old_task, old_artifact, _ = self.observe(self.assignee)
        new_task, new_artifact, _ = self.observe(self.replacement)
        self.share_result(old_task)
        self.share_result(new_task)
        rejected_submission = self.submit(created, old_task, old_artifact, self.assignee)
        rejected = self.review(rejected_submission, False)
        first = self.history(created, self.reviewer, limit=1)
        self.assertEqual([entry["reference"] for entry in first["items"]], [created], first)
        cursor = first["next_cursor"]
        self.assertIsInstance(cursor, str)
        self.assertTrue(cursor)
        token_path = n.base / "history-reviewer.token"
        with os.fdopen(os.open(token_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as token_file:
            token_file.write(self.reviewer["token"])
        cli_page = n.cli(token_path, "work-item", "history", created["resource_id"], "--limit", "1")
        self.assertEqual(cli_page["items"], first["items"], cli_page)

        assigned = self.call("WorkItem.Reassign", {
            **target(rejected), "assignee_principal_id": self.replacement["principal_id"],
        })["reference"]
        accepted_submission = self.submit(assigned, new_task, new_artifact, self.replacement)
        accepted = self.review(accepted_submission, True)
        closed = self.call("WorkItem.Close", target(accepted))["reference"]
        self.assertEqual(self.call("WorkItem.Get", {"id": dependent["resource_id"]})["value"]["blockers"], [])
        self.restart()

        second = self.history(created, self.reviewer, limit=1, cursor=cursor)
        self.assertEqual([entry["reference"] for entry in second["items"]], [rejected_submission], second)
        last = self.history(created, self.reviewer, limit=8, cursor=second["next_cursor"])
        self.assertEqual([entry["reference"] for entry in last["items"]], [rejected], last)
        self.assertIsNone(last["next_cursor"], last)

        full = self.history(created, self.reviewer)
        refs = [created, rejected_submission, rejected, assigned, accepted_submission, accepted, closed]
        self.assertEqual([entry["reference"] for entry in full["items"]], refs, full)
        self.assertEqual([entry["value"]["lifecycle"] for entry in full["items"]],
                         ["Ready", "InReview", "Ready", "Ready", "InReview", "Verified", "Closed"])
        self.assertIsNone(full["next_cursor"], full)
        self.assert_metadata_only(full, old_task, old_artifact)
        self.assert_metadata_only(full, new_task, new_artifact)

        original = self.revision(created, self.reviewer)
        self.assertEqual((original["assignee_principal_id"], original["lifecycle"]),
                         (self.assignee["principal_id"], "Ready"), original)
        declined = self.revision(rejected, self.reviewer)
        self.assertFalse(declined["review"]["accepted"], declined)
        self.assertFalse(declined["has_evidence"], declined)
        reassigned = self.revision(assigned, self.reviewer)
        self.assertEqual(reassigned["assignee_principal_id"], self.replacement["principal_id"], reassigned)
        self.assertEqual(reassigned["assignment"]["previous_assignee_principal_id"], self.assignee["principal_id"])
        self.assertTrue(self.revision(accepted, self.reviewer)["review"]["accepted"])
        self.assertEqual(self.revision(dependent, self.reviewer)["blocked_by"], [created["resource_id"]])
        for task, artifact in ((old_task, old_artifact), (new_task, new_artifact)):
            self.call("Task.EvictResults", {**target(task), "reason": "release historical observation bytes"})
            self.denied("ResultEvicted", "Artifact.Read", {"reference": artifact}, self.reviewer)
        self.restart()
        for reference, task, artifact, lifecycle in (
            (rejected_submission, old_task, old_artifact, "InReview"),
            (accepted_submission, new_task, new_artifact, "InReview"),
            (closed, new_task, new_artifact, "Closed"),
        ):
            historical = self.revision(reference, self.reviewer)
            self.assertEqual(historical["lifecycle"], lifecycle, historical)
            evidence = historical["evidence"]
            self.assertEqual((evidence["task_ref"], evidence["artifact_ref"]), (task, artifact), evidence)
            self.assertNotIn("kernel_release", json.dumps(historical))
        self.assertEqual(self.history(created, self.reviewer), full)
        self.verify()

    def test_scope_and_live_permissions_apply_to_historical_data(self):
        item = self.create("Retain review metadata")
        other = self.create("Separate cursor scope")
        abandoned = self.call("WorkItem.Abandon", {**target(other), "reason": "superseded"})["reference"]
        self.assertEqual(self.revision(abandoned)["abandonment"]["reason"], "superseded")
        task, artifact, payload = self.observe(self.assignee)
        pending = self.submit(item, task, artifact, self.assignee)
        token = self.history(item, self.reviewer, limit=1)["next_cursor"]
        self.assertIsInstance(token, str)
        params = {"id": item["resource_id"], "cursor": token}
        exact = {"reference": pending}

        self.denied("PermissionDenied", "WorkItem.History", {"id": item["resource_id"]}, self.outsider)
        self.denied("PermissionDenied", "WorkItem.RevisionGet", exact, self.outsider)
        self.denied("PermissionDenied", "WorkItem.RevisionGet", {"reference": task}, self.reviewer)
        self.denied("PreconditionFailed", "WorkItem.RevisionGet", {
            "reference": {**pending, "digest": "sha256:" + "0" * 64},
        }, self.reviewer)
        self.denied("PreconditionFailed", "WorkItem.RevisionGet", {
            "reference": {**pending, "version": "r999"},
        }, self.reviewer)
        self.denied("CursorExpired", "WorkItem.History", params, self.assignee)
        self.denied("CursorExpired", "WorkItem.History", {**params, "id": other["resource_id"]}, self.reviewer)
        forged = ("A" if token[0] != "A" else "B") + token[1:]
        self.denied("CursorExpired", "WorkItem.History", {**params, "cursor": forged}, self.reviewer)
        for invalid_limit in (0, 65):
            self.denied("InvalidArgument", "WorkItem.History", {**params, "limit": invalid_limit}, self.reviewer)

        hidden = self.revision(pending, self.reviewer)
        self.assertTrue(hidden["has_evidence"], hidden)
        self.assertIsNone(hidden.get("evidence"), hidden)
        self.assert_metadata_only(hidden, task, artifact)
        rights = self.share_result(task)
        self.assertEqual(self.call("Artifact.Read", {"reference": artifact}, self.reviewer), payload)
        permitted = self.revision(pending, self.reviewer)
        self.assertEqual(permitted["evidence"]["artifact_ref"], artifact)
        self.assertNotIn("kernel_release", json.dumps(permitted))

        grant_cursor = self.history(item, self.reviewer, limit=1)["next_cursor"]
        self.call("Grant.Revoke", target(rights["grant_ref"]))
        self.denied("CursorExpired", "WorkItem.History", {**params, "cursor": grant_cursor}, self.reviewer)
        self.assertEqual(self.revision(pending, self.reviewer), hidden)
        self.denied("PermissionDenied", "Artifact.Read", {"reference": artifact}, self.reviewer)
        self.share_result(task)
        live_cursor = self.history(item, self.reviewer, limit=1)["next_cursor"]
        resume = {**params, "cursor": live_cursor}
        self.member(self.reviewer, None)
        self.denied("PermissionDenied", "WorkItem.History", resume, self.reviewer)
        self.denied("PermissionDenied", "WorkItem.RevisionGet", exact, self.reviewer)
        self.assertEqual(self.call("Artifact.Read", {"reference": artifact}, self.reviewer), payload)
        self.restart()
        self.denied("PermissionDenied", "WorkItem.RevisionGet", exact, self.reviewer)
        self.assertEqual(self.revision(pending, self.assignee)["evidence"]["artifact_ref"], artifact)
        self.member(self.reviewer, "Reviewer")
        self.denied("CursorExpired", "WorkItem.History", resume, self.reviewer)
        self.assertEqual(self.revision(pending, self.reviewer), permitted)
        self.verify()


if __name__ == "__main__":
    unittest.main()
