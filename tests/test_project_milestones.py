"""Exercise milestone acceptance against actual WorkItem closures over Unix RPC."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
import unittest

from support import Failure, Node, key, target


class ProjectMilestoneTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.manager = self.register("milestone-manager")
        self.assignee = self.register("milestone-contributor")
        self.reviewer = self.register("milestone-reviewer")
        self.reader = self.register("milestone-viewer")
        self.project = self.create_project("Review actual host observations")
        self.profile = self.node.get("profile-local")["reference"]
        for actor, role, actions in (
            (self.manager, "Manager", ["project.manage"]),
            (self.assignee, "Contributor", ["project.contribute"]),
            (self.reviewer, "Reviewer", ["project.review", "project.manage"]),
            (self.reader, "Viewer", []),
        ):
            self.member(self.project, actor, role)
            self.grant(actor["principal_id"], ["project.read", *actions], [self.project])

    def register(self, name):
        return self.call("Agent.Register", {"name": name})

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor else None)

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.call(method, params, actor)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def grant(self, principal, actions, targets):
        return self.call("Grant.Issue", {
            "principal_id": principal, "actions": actions, "tool_targets": targets,
            "units": 8, "ttl_seconds": 300,
        })

    def create_project(self, title):
        project = self.call("Project.Create", {"key": key(), "title": title})["reference"]["resource_id"]
        self.grant("local-owner", ["project.read", "project.manage"], [project])
        return project

    def member(self, project, actor, role):
        current = self.call("Project.Get", {"id": project})["reference"]
        self.call("Project.SetMember", {**target(current), "principal_id": actor["principal_id"], "role": role})

    def item(self, project, title):
        current = self.call("Project.Get", {"id": project})["reference"]
        return self.call("WorkItem.Create", {
            "project_id": project, "expected_project_version": current["version"],
            "key": key(), "title": title, "assignee_principal_id": self.assignee["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
        })

    def milestone_request(self, project, items, title):
        current = self.call("Project.Get", {"id": project})["reference"]
        return {
            "project_id": project, "expected_project_version": current["version"], "key": key(),
            "title": title, "description": "Close only after formal review of all nominated work.",
            "work_item_ids": [item["reference"]["resource_id"] for item in items],
        }

    def create_milestone(self, request):
        return self.call("Milestone.Create", request, self.manager)

    def get_milestone(self, milestone, actor=None):
        return self.call("Milestone.Get", {"id": milestone["reference"]["resource_id"]}, actor)

    def get_item(self, item):
        return self.call("WorkItem.Get", {"id": item["reference"]["resource_id"]})

    def observe(self, grant):
        token = self.assignee["token"]
        request = self.node.prepare("system.status", {}, token=token,
                                    delegation=grant["delegation_ref"]["resource_id"])
        task_id = self.call("Task.Submit", request, self.assignee)["task_id"]
        result = self.node.wait(task_id)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Succeeded", "Known"), result)
        artifact, = result["result"]["artifact_refs"]
        payload = self.call("Artifact.Read", {"reference": artifact}, self.assignee)
        self.assertTrue(payload["authority_id"] and payload["kernel_release"], payload)
        return self.node.get(task_id)["reference"], artifact, payload

    def submit_evidence(self, item, task, artifact):
        return self.call("WorkItem.SubmitEvidence", {
            **target(item["reference"]), "task_ref": task, "artifact_ref": artifact,
        }, self.assignee)

    def compete(self, method, request):
        gate = Barrier(2)

        def call(_):
            gate.wait(timeout=5)
            try:
                return self.call(method, request, self.manager)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(max_workers=2) as workers:
            return list(workers.map(call, range(2)))

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 83, report)
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_close_records_exact_independently_accepted_work_items_once(self):
        first = self.item(self.project, "Fresh host observation")
        second = self.item(self.project, "Follow-up host observation")
        nominated = sorted([first, second], key=lambda item: item["reference"]["resource_id"], reverse=True)
        request = self.milestone_request(self.project, nominated, "Local inspection complete")
        opened, duplicate = self.compete("Milestone.Create", request)
        self.assertIsInstance(opened, dict, opened)
        self.assertEqual(duplicate, opened)
        view = opened["value"]
        ids = sorted(request["work_item_ids"])
        self.assertEqual((view["lifecycle"], view["work_item_ids"]), ("Open", ids))
        self.assertEqual(view["unresolved_work_items"], [{"id": item_id, "lifecycle": "Ready"} for item_id in ids])
        self.assertEqual(view["created_by_principal_id"], self.manager["principal_id"])
        self.assertIsNone(view["acceptance"], view)
        self.assertIsNone(view["abandonment"], view)
        self.assertEqual(self.get_milestone(opened, self.reader), opened)
        reader_token = self.node.base / "milestone-reader.token"
        with os.fdopen(os.open(reader_token, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as token_file:
            token_file.write(self.reader["token"])
        cli = self.node.cli(reader_token, "milestone", "get", opened["reference"]["resource_id"])
        self.assertEqual(cli, opened)
        close = target(opened["reference"])
        self.denied("PermissionDenied", "Milestone.Close", close, self.reviewer)
        self.denied("PreconditionFailed", "Milestone.Close", close, self.manager)

        execution = self.grant(self.assignee["principal_id"], ["task.submit", "task.read", "system.status"],
                               [self.profile["resource_id"]])
        closed_items = []
        observations = []
        for item in (first, second):
            task, artifact, payload = self.observe(execution)
            observations.append((task, artifact))
            pending = self.submit_evidence(item, task, artifact)
            review = {**target(pending["reference"]), "accept": True}
            self.denied("PermissionDenied", "WorkItem.Review", review, self.reviewer)
            self.grant(self.reviewer["principal_id"], ["task.results.read"], [task["resource_id"]])
            self.assertEqual(self.call("Artifact.Read", {"reference": artifact}, self.reviewer), payload)
            verified = self.call("WorkItem.Review", review, self.reviewer)
            current = self.get_milestone(opened, self.reader)
            self.assertEqual(current["reference"], opened["reference"], current)
            self.assertIn({"id": item["reference"]["resource_id"], "lifecycle": "Verified"},
                          current["value"]["unresolved_work_items"])
            self.denied("PreconditionFailed", "Milestone.Close", close, self.manager)
            closed_item = self.call("WorkItem.Close", target(verified["reference"]), self.manager)
            self.assertEqual(closed_item["value"]["lifecycle"], "Closed", closed_item)
            closed_items.append(closed_item["reference"])
        ready = self.get_milestone(opened, self.reader)
        self.assertEqual(ready["value"]["unresolved_work_items"], [], ready)
        self.assertEqual(ready["value"]["lifecycle"], "Open", ready)

        self.restart()
        outcomes = self.compete("Milestone.Close", close)
        self.assertEqual(outcomes.count("Conflict"), 1, outcomes)
        closed, = [outcome for outcome in outcomes if isinstance(outcome, dict)]
        self.assertEqual(closed["value"]["lifecycle"], "Closed", closed)
        acceptance = closed["value"]["acceptance"]
        self.assertEqual(acceptance["closed_by_principal_id"], self.manager["principal_id"], acceptance)
        self.assertTrue(acceptance["closed_at"], acceptance)
        self.assertCountEqual(acceptance["closed_work_item_refs"], closed_items)
        self.assertEqual(self.get_milestone(opened, self.reader), closed)
        self.denied("PreconditionFailed", "Milestone.Close", target(closed["reference"]), self.manager)
        self.denied("PreconditionFailed", "Milestone.Abandon", {
            **target(closed["reference"]), "reason": "withdraw accepted work",
        }, self.manager)
        milestone = json.dumps(closed)
        for task, artifact in observations:
            self.call("Task.EvictResults", {**target(task), "reason": "release accepted observation bytes"})
            self.denied("ResultEvicted", "Artifact.Read", {"reference": artifact}, self.assignee)
            self.assertNotIn(task["resource_id"], milestone)
            self.assertNotIn(artifact["resource_id"], milestone)
        self.restart()
        self.assertEqual(self.get_milestone(opened, self.reader), closed)
        audit = self.call("Audit.List", {"limit": 128})
        successful_closures = [entry["actor"] for entry in audit if entry["method"] == "Milestone.Close"
                               and entry["resource"] == closed["reference"]["resource_id"]
                               and entry["decision"] == "Allowed"]
        self.assertEqual(successful_closures, [self.manager["principal_id"]])
        self.member(self.project, self.reader, "Contributor")
        self.assertEqual(self.create_milestone(request), closed)
        self.denied("Conflict", "Milestone.Create", {**request, "key": key()}, self.manager)
        self.denied("IdempotencyConflict", "Milestone.Create", {**request, "title": "Different intent"}, self.manager)
        self.assertEqual(self.call("Milestone.List", {"project_id": self.project}, self.reader)["items"], [closed])
        self.verify()

    def test_scope_fixed_membership_and_abandonment_do_not_replace_closure(self):
        alpha = self.item(self.project, "Keep this work independent")
        beta = self.item(self.project, "Retire this work before acceptance")
        other_project = self.create_project("Independent project")
        self.member(other_project, self.assignee, "Contributor")
        self.member(other_project, self.manager, "Manager")
        other_work = self.item(other_project, "A foreign work item")
        other_request = self.milestone_request(other_project, [other_work], "Foreign milestone")
        foreign = self.call("Milestone.Create", other_request)
        self.denied("PermissionDenied", "Milestone.Create", other_request, self.manager)
        self.denied("PermissionDenied", "Milestone.Get", {"id": foreign["reference"]["resource_id"]}, self.manager)
        self.denied("PermissionDenied", "Milestone.List", {"project_id": other_project}, self.manager)

        request = self.milestone_request(self.project, [alpha, beta], "Retire this milestone")
        ids = request["work_item_ids"]
        for invalid_ids in ([], [ids[0], ids[0]], [ids[0], other_work["reference"]["resource_id"]],
                            [ids[0], "work-item-does-not-exist"], [f"work-item-{i}" for i in range(33)]):
            self.denied("InvalidArgument", "Milestone.Create", {**request, "work_item_ids": invalid_ids},
                        self.manager)
        opened = self.create_milestone(request)
        other = self.create_milestone(self.milestone_request(self.project, [beta], "Keep this milestone open"))
        query = {"project_id": self.project, "limit": 1}
        first_page = self.call("Milestone.List", query, self.reader)
        first, = first_page["items"]
        self.assertEqual(first_page["next_after_id"], first["reference"]["resource_id"])
        last_page = self.call("Milestone.List", {**query, "after_id": first_page["next_after_id"]}, self.reader)
        last, = last_page["items"]
        self.assertCountEqual([first["reference"], last["reference"]], [opened["reference"], other["reference"]])
        self.assertIsNone(last_page["next_after_id"], last_page)
        self.denied("InvalidArgument", "Milestone.List", {**query, "after_id": foreign["reference"]["resource_id"]},
                    self.reader)
        self.denied("InvalidArgument", "Milestone.List", {**query, "limit": 0}, self.reader)

        self.member(self.project, self.reader, None)
        self.denied("PermissionDenied", "Milestone.Get", {"id": opened["reference"]["resource_id"]}, self.reader)
        self.denied("PermissionDenied", "Milestone.List", query, self.reader)
        self.member(self.project, self.reader, "Viewer")
        self.assertEqual(self.get_milestone(opened, self.reader), opened)
        where = {"id": opened["reference"]["resource_id"]}
        self.denied("PermissionDenied", "Resource.Get", where)
        self.denied("PermissionDenied", "Resource.Resolve", {"reference": opened["reference"]})
        self.assertEqual(self.call("Resource.List", {"kind": "Milestone"})["items"], [])

        reason = "Move these objectives to a later release"
        abandon = {**target(opened["reference"]), "reason": reason}
        self.denied("PermissionDenied", "Milestone.Abandon", abandon, self.reviewer)
        self.denied("InvalidArgument", "Milestone.Abandon", {**abandon, "reason": "  "}, self.manager)
        abandoned = self.call("Milestone.Abandon", abandon, self.manager)
        self.assertEqual(abandoned["value"]["lifecycle"], "Abandoned", abandoned)
        self.assertIsNone(abandoned["value"]["acceptance"], abandoned)
        returned = abandoned["value"]["abandonment"]
        self.assertEqual((returned["reason"], returned["abandoned_by_principal_id"]),
                         (reason, self.manager["principal_id"]), returned)
        self.assertTrue(returned["abandoned_at"], returned)
        self.assertEqual(self.get_item(alpha), alpha)
        self.assertEqual(self.get_item(beta), beta)

        retired = self.call("WorkItem.Abandon", {**target(beta["reference"]), "reason": "no longer needed"})
        self.assertEqual(retired["value"]["lifecycle"], "Abandoned", retired)
        remaining = self.get_milestone(other, self.reader)
        self.assertEqual(remaining["value"]["unresolved_work_items"], [
            {"id": beta["reference"]["resource_id"], "lifecycle": "Abandoned"},
        ], remaining)
        self.assertEqual(remaining["value"]["lifecycle"], "Open", remaining)
        self.restart()
        self.assertEqual(self.get_item(beta), retired)
        ended = self.get_milestone(opened, self.reader)
        self.assertEqual(ended["reference"], abandoned["reference"], ended)
        self.assertEqual(ended["value"]["abandonment"], returned, ended)
        self.assertEqual(self.create_milestone(request), ended)
        self.denied("Conflict", "Milestone.Abandon", abandon, self.manager)
        self.denied("PreconditionFailed", "Milestone.Close", target(ended["reference"]), self.manager)
        self.denied("PreconditionFailed", "Milestone.Close", target(other["reference"]), self.manager)
        self.verify()


if __name__ == "__main__":
    unittest.main()
