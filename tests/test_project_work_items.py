"""Exercise local Project assignment, exact Task evidence, and independent review."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
import unittest

from support import Failure, Node, key, target


class ProjectWorkItemTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        # The contributor and reviewer intentionally share a display name.
        self.contributor = self.actor("project-participant")
        self.reviewer = self.actor("project-participant")
        self.outside = self.actor("project-room-guest")
        self.assertNotEqual(self.contributor["principal_id"], self.reviewer["principal_id"])

    def actor(self, name):
        actor = self.node.call("Agent.Register", {"name": name})
        self.grant(actor, ["chat.read", "chat.send", "resource.read", "event.read", "task.read"])
        return actor

    def grant(self, actor, actions, targets=None):
        request = {"principal_id": actor["principal_id"], "actions": actions, "units": 16, "ttl_seconds": 300}
        if targets is not None:
            request["tool_targets"] = targets
        return self.node.call("Grant.Issue", request)

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor else None)

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as rejected:
            self.call(method, params, actor)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    def create_room(self):
        self.grant({"principal_id": "local-owner"}, ["chat.invite", "chat.read"])
        return self.call("Conversation.OpenRoom", {
            "key": key(), "name": "independent-room", "capacity": 4, "ttl_seconds": 180,
        })

    def join_room(self, room, actor):
        invitation = self.call("Conversation.Invite", {
            "id": room["id"], "expected_resource_version": room["resource_version"],
            "principal_id": actor["principal_id"], "ttl_seconds": 120,
        })
        return self.call("Conversation.Respond", {
            "id": room["id"], "expected_resource_version": invitation["conversation"]["resource_version"],
            "membership_id": invitation["membership_id"], "accept": True,
        }, actor)

    def run_task(self, actor, grant, tool):
        request = self.node.prepare(tool, {}, token=actor["token"], delegation=grant["delegation_ref"]["resource_id"])
        task_id = self.call("Task.Submit", request, actor)["task_id"]
        task = self.node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        return self.node.get(task_id)["reference"], artifact

    def read_artifact(self, reference, actor=None):
        return self.call("Artifact.Read", {"reference": reference}, actor)

    def collision(self, method, first, second, actor=None):
        gate = Barrier(2)

        def request(params):
            gate.wait(timeout=5)
            try:
                return self.call(method, params, actor)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(max_workers=2) as workers:
            return list(workers.map(request, (first, second)))

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 80)
        self.assertFalse(report["migration_required"], report)

    def member(self, project_id, actor, role):
        project = self.call("Project.Get", {"id": project_id})
        return self.call("Project.SetMember", {
            **target(project["reference"]), "principal_id": actor["principal_id"], "role": role,
        })

    def test_assignment_result_provenance_and_two_principal_acceptance_survive_restart(self):
        n = self.node
        contributor, reviewer, outside = self.contributor, self.reviewer, self.outside
        project_input = {
            "key": key(), "title": "Local operations", "description": "Verify a recorded host observation.",
        }
        project = self.call("Project.Create", project_input)
        project_id = project["reference"]["resource_id"]
        self.denied("PermissionDenied", "Project.Get", {"id": project_id})
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [project_id])
        self.assertEqual(self.collision("Project.Create", project_input, project_input), [project, project])
        self.denied("IdempotencyConflict", "Project.Create", {**project_input, "title": "Different project"})
        self.member(project_id, contributor, "Contributor")
        self.member(project_id, reviewer, "Reviewer")
        self.denied("PermissionDenied", "Project.Get", {"id": project_id}, contributor)
        self.denied("InvalidArgument", "Grant.Issue", {
            "principal_id": contributor["principal_id"], "actions": ["project.read"],
            "units": 1, "ttl_seconds": 300,
        })
        contributor_rights = self.grant(contributor, ["project.read", "project.contribute", "project.review"],
                                        [project_id])
        self.grant(reviewer, ["project.read", "project.review"], [project_id])
        self.grant(outside, ["project.read"], [project_id])
        reviewer_view = self.call("Project.Get", {"id": project_id}, reviewer)
        self.assertEqual(reviewer_view["reference"]["resource_id"], project_id)

        room = self.create_room()
        room_id = {"id": room["id"]}
        self.denied("PermissionDenied", "Conversation.Get", room_id, reviewer)
        self.join_room(room, contributor)
        self.join_room(self.call("Conversation.Get", room_id), outside)
        self.assertEqual(self.call("Conversation.Get", room_id, outside)["lifecycle"], "Active")
        self.denied("PermissionDenied", "Project.Get", {"id": project_id}, outside)

        profile = n.get("profile-local")["reference"]
        current_project = self.call("Project.Get", {"id": project_id})
        item_input = {
            "project_id": project_id,
            "expected_project_version": current_project["reference"]["version"],
            "key": key(),
            "title": "Capture local status",
            "assignee_principal_id": contributor["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        }
        item, duplicate = self.collision("WorkItem.Create", item_input, item_input)
        self.assertIsInstance(item, dict, item)
        self.assertEqual(duplicate, item)
        self.denied("IdempotencyConflict", "WorkItem.Create", {**item_input, "title": "Different work item"})
        self.assertEqual(item["value"]["lifecycle"], "Ready", item)
        item_id = {"id": item["reference"]["resource_id"]}
        listed = self.call("WorkItem.List", {"project_id": project_id}, contributor)
        self.assertEqual([work["reference"] for work in listed["items"]], [item["reference"]], listed)
        self.denied("InvalidArgument", "WorkItem.Create", {**item_input, "blocked_by": [item_id["id"]]})
        follow_up = self.call("WorkItem.Create", {
            **item_input, "key": key(), "title": "Check the next status observation", "blocked_by": [item_id["id"]],
        })
        follow_up_id = {"id": follow_up["reference"]["resource_id"]}
        self.assertEqual(follow_up["value"]["blockers"], [{**item_id, "lifecycle": "Ready"}], follow_up)
        page = self.call("WorkItem.List", {"project_id": project_id, "limit": 1}, contributor)
        first_item, = page["items"]
        self.assertEqual(first_item["reference"]["resource_id"], page["next_after_id"], page)
        second_page = self.call("WorkItem.List", {
            "project_id": project_id, "limit": 1, "after_id": page["next_after_id"],
        }, contributor)
        second_item, = second_page["items"]
        self.assertCountEqual([first_item["reference"], second_item["reference"]],
                              [item["reference"], follow_up["reference"]])
        self.assertIsNone(second_page["next_after_id"])
        self.denied("PermissionDenied", "WorkItem.Get", item_id, outside)
        self.denied("PermissionDenied", "WorkItem.List", {"project_id": project_id}, outside)
        self.denied("PermissionDenied", "Resource.Resolve", {"reference": item["reference"]}, reviewer)
        self.assertEqual(self.call("Resource.List", {"kind": "WorkItem"}, reviewer)["items"], [])
        self.denied("PreconditionFailed", "WorkItem.Close", target(item["reference"]))

        self.denied("PermissionDenied", "Task.Prepare", {
            "key": key(), "tool": "system.status", "input": {},
            "delegation_id": contributor_rights["delegation_ref"]["resource_id"],
        }, contributor)
        authority = self.call("Node.Get", {})["reference"]["resource_id"]
        tool_grant = self.grant(contributor, ["system.status", "node.inventory", "task.submit", "task.read"],
                                [profile["resource_id"], authority])
        task, output = self.run_task(contributor, tool_grant, "system.status")
        other_task, other_output = self.run_task(contributor, tool_grant, "system.status")
        wrong_task, wrong_output = self.run_task(contributor, tool_grant, "node.inventory")
        payload = self.read_artifact(output, contributor)
        self.assertEqual(payload["authority_id"], authority)
        self.assertNotEqual(other_task, task)

        next_evidence = {**target(follow_up["reference"]), "task_ref": other_task, "artifact_ref": other_output}
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", next_evidence, contributor)

        evidence = {**target(item["reference"]), "task_ref": task, "artifact_ref": output}
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", {
            **evidence, "task_ref": wrong_task, "artifact_ref": wrong_output,
        }, contributor)
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", {
            **evidence, "artifact_ref": other_output,
        }, contributor)
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", {
            **evidence, "artifact_ref": {**output, "digest": "sha256:" + "0" * 64},
        }, contributor)
        self.assertEqual(self.call("WorkItem.Get", item_id, contributor)["reference"], item["reference"])

        watch = self.call("Resource.List", {"kind": "WorkItem"}, reviewer)["watch_start_cursor"]
        submitted = self.call("WorkItem.SubmitEvidence", evidence, contributor)
        self.assertEqual(submitted["value"]["lifecycle"], "InReview", submitted)
        self.assertTrue(submitted["value"]["has_evidence"], submitted)
        self.assertEqual(self.call("Event.Poll", {"cursor": watch}, reviewer)["events"], [])
        evict = {**target(task), "reason": "release old status bytes"}
        self.denied("Conflict", "Task.EvictResults", evict)

        review = {**target(submitted["reference"]), "accept": True}
        self.denied("PermissionDenied", "Artifact.Read", {"reference": output}, reviewer)
        self.denied("PermissionDenied", "WorkItem.Review", review, reviewer)
        projected = self.call("WorkItem.Get", item_id, reviewer)
        self.assertTrue(projected["value"]["has_evidence"], projected)
        self.assertIsNone(projected["value"].get("evidence"), projected)
        self.assertNotIn(output["resource_id"], json.dumps(projected))

        # Give the submitter the review role and capability to exercise principal separation.
        self.member(project_id, contributor, "Reviewer")
        self.assertEqual(self.call("Project.Get", {"id": project_id}, contributor)["value"]["role"], "Reviewer")
        self.assertEqual(self.read_artifact(output, contributor), payload)
        self.denied("PermissionDenied", "WorkItem.Review", review, contributor)
        self.member(project_id, contributor, "Contributor")

        share = self.grant(reviewer, ["task.results.read"], [task["resource_id"]])
        self.assertEqual(self.read_artifact(output, reviewer), payload)
        self.denied("PermissionDenied", "Artifact.Read", {"reference": other_output}, reviewer)
        self.call("Grant.Revoke", target(share["grant_ref"]))
        self.assertEqual(self.call("WorkItem.Get", item_id, reviewer)["reference"], submitted["reference"])
        self.denied("PermissionDenied", "WorkItem.Review", review, reviewer)
        replacement = self.grant(reviewer, ["task.results.read"], [task["resource_id"]])

        n.stop(signal.SIGKILL)
        n.start()
        recovered = self.call("WorkItem.Get", item_id, reviewer)
        self.assertEqual(recovered["reference"], submitted["reference"], recovered)
        self.assertEqual(recovered["value"]["lifecycle"], "InReview", recovered)
        saved = recovered["value"]["evidence"]
        self.assertEqual((saved["task_ref"], saved["artifact_ref"], saved["submitted_by"]),
                         (task, output, contributor["principal_id"]), saved)
        self.assertEqual(self.read_artifact(output, reviewer), payload)
        self.denied("Conflict", "Task.EvictResults", evict)

        outcomes = self.collision("WorkItem.Review", review, review, reviewer)
        self.assertEqual(outcomes.count("Conflict"), 1, outcomes)
        verified, = [outcome for outcome in outcomes if isinstance(outcome, dict)]
        self.assertEqual(verified["value"]["lifecycle"], "Verified", verified)
        self.assertEqual(verified["value"]["review"]["reviewer_principal_id"], reviewer["principal_id"], verified)
        self.assertTrue(verified["value"]["review"]["accepted"], verified)
        self.denied("Conflict", "Task.EvictResults", evict)
        self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", next_evidence, contributor)
        self.denied("PermissionDenied", "WorkItem.Close", target(verified["reference"]), reviewer)
        closed = n.cli(n.witness / "owner.token", "work-item", "close", item_id["id"],
                       "--revision", verified["reference"]["version"])
        self.assertEqual(closed["value"]["lifecycle"], "Closed", closed)

        self.assertEqual(self.call("WorkItem.Get", follow_up_id, contributor)["value"]["blockers"], [])
        next_review = self.call("WorkItem.SubmitEvidence", next_evidence, contributor)
        self.grant(reviewer, ["task.results.read"], [other_task["resource_id"]])
        rejected = self.call("WorkItem.Review", {**target(next_review["reference"]), "accept": False}, reviewer)
        self.assertEqual(rejected["value"]["lifecycle"], "Ready", rejected)
        self.assertFalse(rejected["value"]["has_evidence"], rejected)
        self.assertFalse(rejected["value"]["review"]["accepted"], rejected)
        self.call("Task.EvictResults", {**target(other_task), "reason": "release rejected observation"})
        self.denied("ResultEvicted", "Artifact.Read", {"reference": other_output}, contributor)

        # Removing Project membership leaves the separately granted Task reference readable.
        self.member(project_id, reviewer, None)
        self.denied("PermissionDenied", "Project.Get", {"id": project_id}, reviewer)
        self.denied("PermissionDenied", "WorkItem.Get", item_id, reviewer)
        self.assertEqual(self.read_artifact(output, reviewer), payload)
        self.call("Grant.Revoke", target(replacement["grant_ref"]))
        self.denied("PermissionDenied", "Artifact.Read", {"reference": output}, reviewer)
        self.call("Task.EvictResults", evict)
        self.denied("ResultEvicted", "Artifact.Read", {"reference": output}, contributor)

        n.stop(signal.SIGKILL)
        n.start()
        self.denied("PermissionDenied", "WorkItem.Get", item_id, reviewer)
        final = self.call("WorkItem.Get", item_id)
        self.assertEqual(final["reference"], closed["reference"], final)
        self.assertEqual(final["value"]["lifecycle"], "Closed", final)
        self.assertEqual(final["value"]["review"], closed["value"]["review"], final)
        self.verify()


if __name__ == "__main__":
    unittest.main()
