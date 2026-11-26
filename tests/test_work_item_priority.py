"""Exercise revisioned WorkItem priorities and live minimum-priority tool discovery."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
import unittest

from support import Failure, Node, key, target
from test_agent_assigned_work import ASSIGNED, DISCOVER, STATUS


REPRIORITIZE = "WorkItem.Reprioritize"


class WorkItemPriorityTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.manager = self.call("Agent.Register", {"name": "priority-manager"})
        self.contributor = self.call("Agent.Register", {"name": "priority-contributor"})
        self.reviewer = self.call("Agent.Register", {"name": "priority-independent-reviewer"})
        self.other = self.call("Agent.Register", {"name": "priority-other-contributor"})
        self.profile = self.node.get("profile-local")["reference"]
        self.project = self.call("Project.Create", {"key": key(), "title": "Prioritize verified observations"})[
            "reference"]["resource_id"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [self.project])
        for agent, role, actions in (
            (self.manager, "Manager", ["project.manage"]),
            (self.contributor, "Contributor", ["project.contribute"]),
            (self.reviewer, "Reviewer", ["project.review", "project.manage"]),
            (self.other, "Contributor", ["project.contribute"]),
        ):
            self.member(agent, role)
            self.grant(agent, ["project.read", *actions], [self.project])

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor else None)

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.call(method, params, actor)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def grant(self, actor, actions, targets):
        return self.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions, "tool_targets": targets,
            "units": 8, "ttl_seconds": 300,
        })

    def project_ref(self):
        return self.call("Project.Get", {"id": self.project})["reference"]

    def member(self, actor, role):
        return self.call("Project.SetMember", {
            **target(self.project_ref()), "principal_id": actor["principal_id"], "role": role,
        })["reference"]

    def item(self, title, *, priority=None, assignee=None, blockers=()):
        request = {
            "project_id": self.project, "expected_project_version": self.project_ref()["version"],
            "key": key(), "title": title, "assignee_principal_id": (assignee or self.contributor)["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": STATUS, "target_ref": self.profile},
            "blocked_by": [blocker["reference"]["resource_id"] for blocker in blockers],
        }
        if priority is not None:
            request["priority"] = priority
        return request, self.call("WorkItem.Create", request)

    @staticmethod
    def priority(item):
        return item["value"].get("priority", "Normal")

    def get(self, item):
        return self.call("WorkItem.Get", {"id": item["reference"]["resource_id"]})

    def assigned(self, **filters):
        return self.call(ASSIGNED, {"project_id": self.project, **filters}, self.contributor)

    def set_priority(self, item, priority):
        return self.call(REPRIORITIZE, {**target(item["reference"]), "priority": priority}, self.manager)

    def submit(self, tool, inputs, grant):
        request = self.node.prepare(tool, inputs, token=self.contributor["token"],
                                    delegation=grant["delegation_ref"]["resource_id"])
        return request, self.call("Task.Submit", request, self.contributor)["task_id"]

    def result(self, task_id):
        state = self.node.wait(task_id)
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"]), ("Succeeded", "Known"), state)
        artifact, = state["result"]["artifact_refs"]
        payload = self.call("Artifact.Read", {"reference": artifact}, self.contributor)
        return self.node.get(task_id)["reference"], artifact, payload

    def schedule(self, mode):
        node = self.call("Node.Get", {})["reference"]
        self.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 89, report)
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_reprioritization_is_a_scoped_ready_only_revision_with_one_cas_winner(self):
        creation, original = self.item("Observe a pending availability issue")
        item_id = original["reference"]["resource_id"]
        self.assertEqual(self.priority(original), "Normal", original)
        self.assertNotIn("priority", original["value"])
        self.assertEqual(self.call("WorkItem.Create", {**creation, "priority": "Normal"}), original)
        invalid_claim = {**target(original["reference"]), "priority": "Urgent"}
        self.denied("PermissionDenied", REPRIORITIZE, invalid_claim, self.reviewer)
        self.denied("PermissionDenied", REPRIORITIZE, invalid_claim, self.contributor)
        self.denied("InvalidArgument", REPRIORITIZE, {**invalid_claim, "priority": "Critical"}, self.manager)
        other_project = self.call("Project.Create", {"key": key(), "title": "Different priority authority"})["reference"]
        other_project_id = other_project["resource_id"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [other_project_id])
        for agent, role in ((self.manager, "Manager"), (self.contributor, "Contributor")):
            other_project = self.call("Project.SetMember", {
                **target(other_project), "principal_id": agent["principal_id"], "role": role,
            })["reference"]
        outside = self.call("WorkItem.Create", {
            **creation, "project_id": other_project_id, "expected_project_version": other_project["version"],
            "key": key(), "title": "Outside the manager's grant",
        })
        self.denied("PermissionDenied", REPRIORITIZE,
                    {**target(outside["reference"]), "priority": "Urgent"}, self.manager)
        self.assertEqual(self.set_priority(original, "Normal"), original)

        gate = Barrier(2)

        def change(priority):
            gate.wait(timeout=5)
            try:
                return self.set_priority(original, priority)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(max_workers=2) as workers:
            outcomes = list(workers.map(change, ("High", "Urgent")))
        self.assertEqual(outcomes.count("Conflict"), 1, outcomes)
        updated, = [result for result in outcomes if isinstance(result, dict)]
        self.assertIn(self.priority(updated), ("High", "Urgent"))
        summary = updated["value"]["priority_change"]
        self.assertEqual((summary["previous_priority"], summary["reprioritized_by_principal_id"]),
                         ("Normal", self.manager["principal_id"]), summary)
        self.assertTrue(summary["reprioritized_at"], summary)
        self.assertEqual(self.set_priority(updated, self.priority(updated)), updated)
        self.denied("Conflict", REPRIORITIZE,
                    {**target(original["reference"]), "priority": self.priority(updated)}, self.manager)

        self.member(self.manager, "Reviewer")
        self.denied("PermissionDenied", REPRIORITIZE,
                    {**target(updated["reference"]), "priority": "Low"}, self.manager)
        self.member(self.manager, "Manager")
        lowered = self.set_priority(updated, "Low")
        token_file = self.node.base / "priority-manager.token"
        with os.fdopen(os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            stream.write(self.manager["token"])
        high = self.node.cli(token_file, "work-item", "reprioritize", item_id,
                             "--revision", lowered["reference"]["version"], "high")
        self.assertEqual(self.priority(high), "High", high)
        self.assertEqual(high["value"]["priority_change"]["previous_priority"], "Low")
        self.assertEqual(self.call("WorkItem.Create", creation), self.get(high))
        self.assertEqual(self.call("WorkItem.Create", {**creation, "priority": "Normal"}), self.get(high))
        self.denied("IdempotencyConflict", "WorkItem.Create", {**creation, "priority": "High"})
        before = self.call("WorkItem.RevisionGet", {"reference": original["reference"]}, self.contributor)
        self.assertEqual(self.priority(before), "Normal", before)
        history = self.call("WorkItem.History", {"id": item_id}, self.contributor)["items"]
        self.assertEqual([entry["reference"] for entry in history],
                         [original["reference"], updated["reference"], lowered["reference"], high["reference"]])
        self.assertEqual([self.priority(entry) for entry in history],
                         ["Normal", self.priority(updated), "Low", "High"])

        execution = self.grant(self.contributor, ["task.submit", "task.read", STATUS], [self.profile["resource_id"]])
        _, task_id = self.submit(STATUS, {}, execution)
        task, artifact, state = self.result(task_id)
        self.assertTrue(state["authority_id"] and state["kernel_release"], state)
        pending = self.call("WorkItem.SubmitEvidence", {
            **target(high["reference"]), "task_ref": task, "artifact_ref": artifact,
        }, self.contributor)
        self.assertEqual(self.priority(pending), "High")
        self.denied("PreconditionFailed", REPRIORITIZE,
                    {**target(pending["reference"]), "priority": "Urgent"}, self.manager)
        self.grant(self.reviewer, ["task.results.read"], [task_id])
        accepted = self.call("WorkItem.Review", {**target(pending["reference"]), "accept": True}, self.reviewer)
        self.denied("PreconditionFailed", REPRIORITIZE,
                    {**target(accepted["reference"]), "priority": "Urgent"}, self.manager)
        closed = self.call("WorkItem.Close", target(accepted["reference"]), self.manager)
        self.assertEqual((closed["value"]["lifecycle"], self.priority(closed)), ("Closed", "High"), closed)
        self.denied("PreconditionFailed", REPRIORITIZE,
                    {**target(closed["reference"]), "priority": "High"}, self.manager)
        _, withdrawn = self.item("Withdraw before review", priority="Urgent")
        abandoned = self.call("WorkItem.Abandon", {**target(withdrawn["reference"]), "reason": "retire observation"})
        self.denied("PreconditionFailed", REPRIORITIZE,
                    {**target(abandoned["reference"]), "priority": "Low"}, self.manager)
        self.restart()
        self.assertEqual(self.get(high)["reference"], closed["reference"])
        self.assertEqual(self.priority(self.get(high)), "High")
        self.assertEqual(self.call("WorkItem.RevisionGet", {"reference": original["reference"]}, self.contributor),
                         before)
        self.verify()

    def test_reprioritization_preserves_the_exact_rejected_review(self):
        _, original = self.item("Revisit a rejected observation", priority="Low")
        execution = self.grant(self.contributor, ["task.submit", "task.read", STATUS], [self.profile["resource_id"]])
        _, task_id = self.submit(STATUS, {}, execution)
        task, artifact, _ = self.result(task_id)
        pending = self.call("WorkItem.SubmitEvidence", {
            **target(original["reference"]), "task_ref": task, "artifact_ref": artifact,
        }, self.contributor)
        self.grant(self.reviewer, ["task.results.read"], [task_id])
        rejected = self.call("WorkItem.Review", {**target(pending["reference"]), "accept": False}, self.reviewer)
        decision = rejected["value"]["review"]
        self.assertFalse(decision["accepted"], rejected)
        self.assertEqual(rejected["value"]["lifecycle"], "Ready")
        promoted = self.set_priority(rejected, "Urgent")
        settled = self.set_priority(promoted, "Normal")
        self.assertEqual(settled["value"]["review"], decision)
        self.restart()
        self.assertEqual(self.get(original)["value"]["review"], decision)
        history = self.call("WorkItem.History", {"id": original["reference"]["resource_id"]})["items"]
        self.assertEqual([entry["reference"] for entry in history[-3:]],
                         [rejected["reference"], promoted["reference"], settled["reference"]])
        self.assertEqual([entry["value"]["review"] for entry in history[-3:]], [decision] * 3)
        self.verify()

    def test_assigned_tool_applies_live_threshold_after_queue_restart(self):
        _, normal = self.item("Default normal")
        _, low = self.item("Low check", priority="Low")
        _, high = self.item("High check", priority="High")
        _, urgent = self.item("Urgent check", priority="Urgent")
        _, elsewhere = self.item("Urgent for another contributor", priority="Urgent", assignee=self.other)
        _, blocked = self.item("Urgent after a prerequisite", priority="Urgent", blockers=[elsewhere])
        _, retired = self.item("Urgent retired work", priority="Urgent")
        self.call("WorkItem.Abandon", {**target(retired["reference"]), "reason": "remove urgent work"})

        def refs(page):
            return [entry["reference"] for entry in page["items"]]

        def expected(*items):
            return sorted((item["reference"] for item in items), key=lambda ref: ref["resource_id"])

        self.assertEqual(refs(self.assigned()), expected(normal, low, high, urgent))
        self.assertEqual(refs(self.assigned(min_priority="Low")), expected(normal, low, high, urgent))
        self.assertEqual(refs(self.assigned(min_priority="Normal")), expected(normal, high, urgent))
        only_high = self.assigned(min_priority="High", limit=1)
        only_high_tail = self.assigned(min_priority="High", limit=1, after_id=only_high["next_after_id"])
        self.assertEqual(refs(only_high) + refs(only_high_tail), expected(high, urgent))
        self.assertIsNone(only_high_tail["next_after_id"])
        self.assertEqual(refs(self.assigned(min_priority="Urgent")), expected(urgent))
        with_blocked = self.assigned(min_priority="Urgent", include_blocked=True)
        self.assertEqual(refs(with_blocked), expected(urgent, blocked))
        waiting, = [item for item in with_blocked["items"] if item["reference"] == blocked["reference"]]
        self.assertEqual(waiting["value"]["blockers"],
                         [{"id": elsewhere["reference"]["resource_id"], "lifecycle": "Ready"}])
        self.denied("InvalidArgument", ASSIGNED, {"project_id": self.project, "min_priority": "Critical"},
                    self.contributor)

        tool_grant = self.grant(self.contributor, [DISCOVER, "task.submit", "task.read"], [self.project])
        self.schedule("Cordoned")
        inputs = {"project_id": self.project, "expected_project_version": self.project_ref()["version"],
                  "min_priority": "High"}
        request, task_id = self.submit(DISCOVER, inputs, tool_grant)
        self.assertEqual(self.call("Task.Get", {"id": task_id})["lifecycle"], "Queued")
        downgraded = self.set_priority(high, "Normal")
        self.assertEqual(downgraded["value"]["priority"], "Normal", downgraded)
        self.assertEqual(downgraded["value"]["priority_change"]["previous_priority"], "High")
        promoted = self.set_priority(normal, "Urgent")
        self.restart()
        self.schedule("Enabled")

        _, artifact, page = self.result(task_id)
        self.assertEqual(refs(page), expected(promoted, urgent), page)
        self.assertEqual(page, self.assigned(min_priority="High"))
        self.assertTrue(all(self.priority(item) == "Urgent" for item in page["items"]), page)
        self.set_priority(promoted, "Low")
        self.assertEqual(refs(self.assigned(min_priority="High")), expected(urgent))
        self.restart()
        replay = self.call("Task.Submit", request, self.contributor)
        self.assertTrue(replay["deduplicated"] and replay["task_id"] == task_id, replay)
        self.assertEqual(self.call("Artifact.Read", {"reference": artifact}, self.contributor), page)
        self.verify()


if __name__ == "__main__":
    unittest.main()
