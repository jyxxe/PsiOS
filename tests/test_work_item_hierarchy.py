"""Exercise private WorkItem hierarchy and its combined closure dependencies."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
import unittest

from support import Failure, Node, key, target


class WorkItemHierarchyTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.contributor = self.call("Agent.Register", {"name": "hierarchy-contributor"})
        self.reviewer = self.call("Agent.Register", {"name": "hierarchy-reviewer"})
        self.viewer = self.call("Agent.Register", {"name": "hierarchy-viewer"})
        self.project = self.new_project("Close the host acceptance hierarchy")
        self.profile = self.node.get("profile-local")["reference"]
        for actor, role, actions in (
            (self.contributor, "Contributor", ["project.contribute"]),
            (self.reviewer, "Reviewer", ["project.review", "project.manage"]),
            (self.viewer, "Viewer", []),
        ):
            self.member(self.project, actor, role)
            self.grant(actor["principal_id"], ["project.read", *actions], [self.project])

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

    def new_project(self, title):
        project = self.call("Project.Create", {"key": key(), "title": title})["reference"]["resource_id"]
        self.grant("local-owner", ["project.read", "project.manage"], [project])
        return project

    def member(self, project, actor, role):
        reference = self.call("Project.Get", {"id": project})["reference"]
        self.call("Project.SetMember", {
            **target(reference), "principal_id": actor["principal_id"], "role": role,
        })

    @staticmethod
    def item_id(item):
        return item["reference"]["resource_id"]

    def request(self, title, *, project=None, parent=None, blockers=()):
        project = project or self.project
        reference = self.call("Project.Get", {"id": project})["reference"]
        request = {
            "project_id": project, "expected_project_version": reference["version"],
            "key": key(), "title": title, "assignee_principal_id": self.contributor["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
        }
        if parent is not None:
            request["parent_id"] = self.item_id(parent)
        if blockers:
            request["blocked_by"] = [self.item_id(blocker) for blocker in blockers]
        return request

    def item(self, title, **options):
        return self.call("WorkItem.Create", self.request(title, **options))

    def get(self, item, actor=None):
        return self.call("WorkItem.Get", {"id": self.item_id(item)}, actor)

    def children(self, item, actor=None, **params):
        return self.call("WorkItem.Children", {"id": self.item_id(item), **params}, actor)

    def execution(self):
        grant = self.grant(self.contributor["principal_id"], ["task.submit", "task.read", "system.status"],
                           [self.profile["resource_id"]])
        return grant["delegation_ref"]["resource_id"]

    def verify_item(self, item, delegation):
        request = self.node.prepare("system.status", {}, token=self.contributor["token"], delegation=delegation)
        task_id = self.call("Task.Submit", request, self.contributor)["task_id"]
        settled = self.node.wait(task_id)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"), settled)
        task = self.node.get(task_id)["reference"]
        artifact, = settled["result"]["artifact_refs"]
        payload = self.call("Artifact.Read", {"reference": artifact}, self.contributor)
        self.assertTrue(payload["authority_id"] and payload["kernel_release"], payload)
        pending = self.call("WorkItem.SubmitEvidence", {
            **target(item["reference"]), "task_ref": task, "artifact_ref": artifact,
        }, self.contributor)
        review = {**target(pending["reference"]), "accept": True}
        self.denied("PermissionDenied", "WorkItem.Review", review, self.reviewer)
        self.grant(self.reviewer["principal_id"], ["task.results.read"], [task_id])
        verified = self.call("WorkItem.Review", review, self.reviewer)
        self.assertEqual(verified["value"]["lifecycle"], "Verified", verified)
        self.assertEqual(verified["value"]["review"]["reviewer_principal_id"], self.reviewer["principal_id"])
        return verified, task

    def compete(self, method, params):
        gate = Barrier(2)

        def call(_):
            gate.wait(timeout=5)
            try:
                return self.call(method, params)
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
        self.assertGreaterEqual(report["database_format"], 85, report)
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_parent_requires_every_independently_verified_child_to_close(self):
        parent_request = self.request("Accept the complete host observation")
        parent = self.call("WorkItem.Create", parent_request)
        self.assertNotIn("parent_id", parent["value"])
        child_request = self.request("Verify the first observation", parent=parent)
        first, replay = self.compete("WorkItem.Create", child_request)
        self.assertIsInstance(first, dict, first)
        self.assertEqual(replay, first)
        second = self.item("Verify a separate observation", parent=parent)
        self.assertEqual(self.get(parent), parent)
        self.assertEqual(first["value"]["parent_id"], self.item_id(parent))

        page = self.children(parent, self.viewer, limit=1)
        entry, = page["items"]
        self.assertEqual(page["next_after_id"], self.item_id(entry))
        rest = self.children(parent, self.viewer, limit=1, after_id=page["next_after_id"])
        listing = page["items"] + rest["items"]
        self.assertEqual([self.item_id(item) for item in listing], sorted([self.item_id(first), self.item_id(second)]))
        self.assertTrue(all(item["value"]["parent_id"] == self.item_id(parent) for item in listing))
        self.assertIsNone(rest["next_after_id"], rest)
        reader_token = self.node.base / "hierarchy-viewer.token"
        with os.fdopen(os.open(reader_token, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as token_file:
            token_file.write(self.viewer["token"])
        self.assertEqual(self.node.cli(reader_token, "work-item", "children", self.item_id(parent), "--limit", "1"),
                         page)

        execution = self.execution()
        verified_parent, parent_task = self.verify_item(parent, execution)
        close_parent = target(verified_parent["reference"])
        self.denied("PermissionDenied", "WorkItem.Close", close_parent, self.reviewer)
        self.denied("PreconditionFailed", "WorkItem.Close", close_parent)
        no_late_child = self.request("Refuse a late checklist entry", parent=parent)
        self.denied("PreconditionFailed", "WorkItem.Create", no_late_child)
        self.assertEqual(self.call("WorkItem.Create", child_request), first)
        self.restart()
        self.assertEqual(self.get(parent, self.reviewer)["reference"], verified_parent["reference"])
        self.denied("PreconditionFailed", "WorkItem.Close", close_parent)
        eviction = {**target(parent_task), "reason": "parent review still retains its evidence"}
        self.denied("Conflict", "Task.EvictResults", eviction)

        closed_children = []
        task_ids = {parent_task["resource_id"]}
        for child in (first, second):
            verified_child, task = self.verify_item(child, execution)
            self.assertNotIn(task["resource_id"], task_ids)
            task_ids.add(task["resource_id"])
            self.denied("PreconditionFailed", "WorkItem.Close", close_parent)
            closed = self.call("WorkItem.Close", target(verified_child["reference"]))
            self.assertEqual(closed["value"]["lifecycle"], "Closed", closed)
            closed_children.append(closed["reference"])
        outcomes = self.compete("WorkItem.Close", close_parent)
        self.assertEqual(outcomes.count("Conflict"), 1, outcomes)
        closed_parent, = [outcome for outcome in outcomes if isinstance(outcome, dict)]
        self.assertEqual(closed_parent["value"]["lifecycle"], "Closed", closed_parent)
        self.denied("PreconditionFailed", "WorkItem.Create", no_late_child)
        self.assertEqual(self.call("WorkItem.Create", parent_request), closed_parent)
        self.assertEqual(self.call("WorkItem.Create", child_request)["reference"], closed_children[0])
        independent = self.item("Another ready parent")
        self.denied("IdempotencyConflict", "WorkItem.Create", {
            **child_request, "parent_id": self.item_id(independent),
        })
        self.denied("IdempotencyConflict", "WorkItem.Create", {
            **parent_request, "parent_id": self.item_id(independent),
        })
        original_child = self.call("WorkItem.RevisionGet", {"reference": first["reference"]}, self.viewer)
        self.assertEqual(original_child["value"]["parent_id"], self.item_id(parent), original_child)
        self.assertEqual(original_child["value"]["lifecycle"], "Ready", original_child)
        child_history = self.call("WorkItem.History", {"id": self.item_id(first)}, self.viewer)["items"]
        self.assertEqual(child_history[0]["reference"], first["reference"])
        self.assertEqual(child_history[-1]["reference"], closed_children[0])
        self.assertTrue(all(revision["value"]["parent_id"] == self.item_id(parent)
                            for revision in child_history))

        self.restart()
        self.assertEqual(self.get(parent), closed_parent)
        recovered = self.children(parent, self.viewer)
        self.assertCountEqual([child["reference"] for child in recovered["items"]], closed_children)
        self.assertTrue(all(child["value"]["lifecycle"] == "Closed" for child in recovered["items"]))
        self.verify()

    def test_combined_cycles_and_foreign_parent_fail_without_changing_the_hierarchy(self):
        parent = self.item("Keep an acyclic closure graph")
        depends_on_parent = self.item("Wait for the parent", blockers=[parent])
        for blockers in ([parent], [depends_on_parent]):
            self.denied("InvalidArgument", "WorkItem.Create",
                        self.request("Would deadlock closure", parent=parent, blockers=blockers))
        self.assertEqual(self.children(parent)["items"], [])
        child = self.item("A legitimate child", parent=parent)
        for ancestor_blocker in (parent, depends_on_parent):
            self.denied("InvalidArgument", "WorkItem.Create", self.request(
                "A longer cycle through the ancestor", parent=child, blockers=[ancestor_blocker],
            ))
        external_blocker = self.item("Independent prerequisite")
        grandchild = self.item("An acyclic grandchild", parent=child, blockers=[external_blocker])
        self.assertEqual([self.item_id(item) for item in self.children(parent)["items"]], [self.item_id(child)])
        self.assertEqual([self.item_id(item) for item in self.children(child)["items"]], [self.item_id(grandchild)])
        self.assertEqual(self.get(grandchild)["value"]["blockers"],
                         [{"id": self.item_id(external_blocker), "lifecycle": "Ready"}])

        foreign_project = self.new_project("Separate project scope")
        self.member(foreign_project, self.contributor, "Contributor")
        self.member(foreign_project, self.viewer, "Viewer")
        foreign = self.item("Independent project item", project=foreign_project)
        query = {"id": self.item_id(parent)}
        self.denied("InvalidArgument", "WorkItem.Create", self.request("No foreign parent", parent=foreign))
        self.denied("PermissionDenied", "WorkItem.Children", {"id": self.item_id(foreign)}, self.viewer)
        for unrelated in (grandchild, depends_on_parent, foreign):
            self.denied("InvalidArgument", "WorkItem.Children", {
                **query, "after_id": self.item_id(unrelated),
            }, self.viewer)
        self.denied("InvalidArgument", "WorkItem.Children", {**query, "limit": 65}, self.viewer)
        self.member(self.project, self.viewer, None)
        self.denied("PermissionDenied", "WorkItem.Children", query, self.viewer)

        held = self.item("An abandoned child leaves this parent incomplete")
        withdrawn = self.item("Withdraw this child", parent=held)
        abandoned = self.call("WorkItem.Abandon", {**target(withdrawn["reference"]), "reason": "scope removed"})
        self.assertEqual(abandoned["value"]["lifecycle"], "Abandoned", abandoned)
        verified, _ = self.verify_item(held, self.execution())
        self.denied("PreconditionFailed", "WorkItem.Close", target(verified["reference"]))
        self.restart()
        self.denied("PermissionDenied", "WorkItem.Children", query, self.viewer)
        self.assertEqual(self.children(held)["items"][0]["reference"], abandoned["reference"])
        self.denied("PreconditionFailed", "WorkItem.Close", target(verified["reference"]))
        self.verify()


if __name__ == "__main__":
    unittest.main()
