"""Discover a contributor's work and satisfy its evidence criterion from a native Agent Session."""
import json
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key, target


ASSIGNED = "WorkItem.Assigned"
DISCOVER = "project.list_assigned_work"
SUBMIT = "project.submit_evidence"
STATUS = "system.status"


class AgentAssignedWorkTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.agent = self.call("Agent.Register", {"name": "assigned-work-contributor"})
        self.other = self.call("Agent.Register", {"name": "different-assignee"})
        self.reviewer = self.call("Agent.Register", {"name": "assigned-work-reviewer"})
        self.profile = self.node.get("profile-local")["reference"]
        self.project = self.create_project("Independently review an assigned observation")
        self.stale_project = self.member(self.project, self.agent, "Contributor")
        self.project_grant = self.contribute(self.project, self.agent)
        self.member(self.project, self.other, "Contributor")
        self.contribute(self.project, self.other)
        self.member(self.project, self.reviewer, "Reviewer")
        self.grant(self.reviewer, ["project.read", "project.review", "project.contribute"], [self.project])

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor else None)

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.call(method, params, actor)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def grant(self, actor, actions, targets, **options):
        return self.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions, "tool_targets": targets,
            "units": 8, "ttl_seconds": 300, **options,
        })

    def create_project(self, title):
        project = self.call("Project.Create", {"key": key(), "title": title})["reference"]["resource_id"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [project])
        return project

    def project_ref(self, project):
        return self.call("Project.Get", {"id": project})["reference"]

    def member(self, project, actor, role):
        return self.call("Project.SetMember", {
            **target(self.project_ref(project)), "principal_id": actor["principal_id"], "role": role,
        })["reference"]

    def contribute(self, project, actor):
        return self.grant(actor, ["project.read", "project.contribute"], [project])

    def create_item(self, title, agent=None, project=None, blockers=()):
        current = self.project_ref(project or self.project)
        return self.call("WorkItem.Create", {
            "project_id": current["resource_id"], "expected_project_version": current["version"],
            "key": key(), "title": title, "assignee_principal_id": (agent or self.agent)["principal_id"],
            "blocked_by": [item["resource_id"] for item in blockers],
            "criterion": {"kind": "TaskResult", "tool": STATUS, "target_ref": self.profile},
        })["reference"]

    def assigned(self, reference, **params):
        return self.call(ASSIGNED, {
            "project_id": reference["resource_id"], "expected_project_version": reference["version"], **params,
        }, self.agent)

    def open_session(self, item):
        self.provider = self.call("Agent.Register", {"name": "assigned-work-model-provider"})
        self.model = self.call("Model.Register", {
            "name": "assigned-work-model", "revision": "1", "provider_principal_id": self.provider["principal_id"],
            "lease_seconds": 60, "max_context_bytes": 32_768, "max_output_bytes": 8192, "max_in_flight": 1,
            "inference_outcome_policy": "Strict",
        })
        self.grant(self.provider, ["model.serve"], [self.model["resource_id"]])
        skill = self.call("Skill.Publish", {
            "name": "fulfilled-assigned-work", "version": "1", "source": "local-test", "license": "0BSD",
            "instructions": "Discover a ready work item, record its observation, and submit evidence for review.",
            "tools": [DISCOVER, STATUS, SUBMIT],
        })
        grant = self.grant(
            self.agent, ["session.run", "task.submit", "task.read", "resource.read", DISCOVER, STATUS, SUBMIT],
            [self.model["resource_id"], skill["resource_id"], self.project,
             self.profile["resource_id"], item["resource_id"]], model_calls=4,
        )
        session = self.call("Session.Open", {
            "key": key(), "goal": "Review one authorized ready observation from the project.",
            "model_ref": self.model, "skill_ref": skill, "max_turns": 4,
            "provider_queue": True, "context_policy": "LocalToolResults", "tool_context_window": 2,
            "delegation_id": grant["delegation_ref"]["resource_id"],
        }, self.agent)["reference"]
        return session["resource_id"], grant["budget_ref"]["resource_id"]

    def claim(self, claim_key):
        return self.call("Model.ClaimNext", {"model_ref": self.model, "key": claim_key}, self.provider)

    def respond(self, request):
        return self.call("Model.Respond", request, self.provider)

    @staticmethod
    def decision(claim, tool, inputs):
        return {"session_id": claim["session_id"], "lease_id": claim["lease_id"],
                "decision": {"action": "tool", "tool": tool, "input": inputs}}

    def result(self, task_id):
        task = self.node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        payload = self.call("Artifact.Read", {"reference": artifact}, self.agent)
        return self.node.get(task_id)["reference"], artifact, payload

    def schedule(self, mode):
        ref = self.call("Node.Get", {})["reference"]
        self.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 88, report)
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_native_model_discovers_then_satisfies_one_exact_work_item(self):
        item = self.create_item("Record the actual local node status")
        blocker = self.create_item("A pending prerequisite", agent=self.other)
        blocked = self.create_item("Needs a completed prerequisite", blockers=[blocker])
        other = self.create_item("Observation assigned to a different contributor", agent=self.other)
        foreign_project = self.create_project("A different authorized project")
        self.member(foreign_project, self.agent, "Contributor")
        self.contribute(foreign_project, self.agent)
        foreign_item = self.create_item("Work elsewhere", project=foreign_project)
        foreign_ref = self.project_ref(foreign_project)
        self.assertEqual([row["reference"] for row in self.assigned(foreign_ref)["items"]], [foreign_item])

        session, budget_id = self.open_session(item)
        project = self.project_ref(self.project)
        inputs = {"project_id": self.project, "expected_project_version": project["version"], "limit": 1}
        first = self.claim("discover-unassigned")
        self.assertEqual((first["session_id"], first["turn"]), (session, 1), first)
        self.assertIsNone(first["context"]["last_tool"], first)
        stale = {**inputs, "expected_project_version": self.stale_project["version"]}
        cross = {**inputs, "project_id": foreign_project, "expected_project_version": foreign_ref["version"]}
        self.denied("Conflict", "Model.Respond", self.decision(first, DISCOVER, stale), self.provider)
        self.denied("PermissionDenied", "Model.Respond", self.decision(first, DISCOVER, cross), self.provider)
        self.call("Grant.Revoke", target(self.project_grant["grant_ref"]))
        request = self.decision(first, DISCOVER, inputs)
        self.denied("PermissionDenied", "Model.Respond", request, self.provider)
        self.assertEqual(self.call("Resource.List", {"kind": "Task"})["items"], [])
        self.contribute(self.project, self.agent)

        self.schedule("Cordoned")
        discovered = self.respond(request)
        task_id = discovered["task_id"]
        queued = self.call("Task.Get", {"id": task_id})
        self.assertEqual((queued["lifecycle"], queued["outcome_knowledge"]), ("Queued", "NotStarted"), queued)
        operation, = queued["operation_refs"]
        plan_ref = self.call("Resource.Resolve", {"reference": operation})["value"]["plan_ref"]
        plan = self.call("Resource.Resolve", {"reference": plan_ref})["value"]
        self.assertEqual([effect["resource_ref"] for effect in plan["allowed_effects"]], [project], plan)
        self.assertEqual([condition["resource_ref"] for condition in plan["preconditions"]], [project], plan)
        self.restart()
        self.assertEqual(self.respond(request), discovered)
        self.assertEqual(self.claim("discover-unassigned")["completed"], discovered)
        self.schedule("Enabled")
        discovery_task, discovery_artifact, page = self.result(task_id)
        self.assertIsNone(page["next_after_id"], page)
        chosen, = page["items"]
        self.assertEqual(chosen["reference"], item, chosen)
        self.assertEqual(chosen["value"]["assignee_principal_id"], self.agent["principal_id"], chosen)
        self.assertIsNone(chosen["value"].get("evidence"), chosen)
        for hidden in (blocker, blocked, other, foreign_item):
            self.assertNotIn(hidden["resource_id"], json.dumps(page))

        self.restart()
        observe = self.claim("observe-selected-work")
        self.assertEqual((observe["session_id"], observe["turn"]), (session, 2), observe)
        listed = observe["context"]["last_tool"]
        self.assertEqual((listed["task_ref"], listed["local_tool_result"]), (discovery_task, page), listed)
        selected = listed["local_tool_result"]["items"][0]
        status = self.respond(self.decision(observe, selected["value"]["criterion"]["tool"], {}))
        task, artifact, observation = self.result(status["task_id"])
        self.assertTrue(observation["authority_id"] and observation["kernel_release"], observation)

        attach = self.claim("attach-selected-work")
        previous, = attach["context"]["previous_tools"]
        current = attach["context"]["last_tool"]
        self.assertEqual(previous["local_tool_result"], page, previous)
        self.assertEqual((current["task_ref"], current["result"]["artifact_refs"], current["local_tool_result"]),
                         (task, [artifact], observation), current)
        retained_item = previous["local_tool_result"]["items"][0]["reference"]
        attached = self.respond(self.decision(attach, SUBMIT, {
            **target(retained_item), "task_ref": current["task_ref"],
            "artifact_ref": current["result"]["artifact_refs"][0],
        }))
        _, _, receipt = self.result(attached["task_id"])
        pending = self.call("WorkItem.Get", {"id": item["resource_id"]}, self.agent)
        self.assertEqual(receipt, {"work_item_ref": pending["reference"], "lifecycle": "InReview"})
        evidence = pending["value"]["evidence"]
        self.assertEqual((evidence["task_ref"], evidence["artifact_ref"]), (task, artifact), evidence)
        final = self.claim("finish-selected-work")
        self.assertEqual(final["context"]["last_tool"]["local_tool_result"], receipt)
        outcome = self.respond({"session_id": session, "lease_id": final["lease_id"],
                                "decision": {"action": "finish", "summary": "Submitted the discovered work."}})
        self.assertEqual(outcome["lifecycle"], "Succeeded", outcome)
        history = self.call("Session.History", {"id": session}, self.agent)
        self.assertEqual([(step["turn"], step["task_id"]) for step in history["tasks"]],
                         [(1, task_id), (2, status["task_id"]), (3, attached["task_id"])])
        budget = self.call("Budget.Get", {"id": budget_id}, self.agent)
        self.assertEqual((budget["model_calls_used"], budget["spent"]), (4, 3), budget)
        self.assertEqual(self.assigned(project)["items"], [])

        review = {**target(pending["reference"]), "accept": True}
        self.denied("PermissionDenied", "WorkItem.Review", review, self.reviewer)
        self.grant(self.reviewer, ["task.results.read"], [task["resource_id"]])
        self.assertEqual(self.call("Artifact.Read", {"reference": artifact}, self.reviewer), observation)
        self.denied("PermissionDenied", "Artifact.Read", {"reference": discovery_artifact}, self.reviewer)
        reviewed = self.call("WorkItem.Review", review, self.reviewer)
        self.assertEqual(reviewed["value"]["lifecycle"], "Verified", reviewed)
        self.assertEqual(reviewed["value"]["review"]["reviewer_principal_id"], self.reviewer["principal_id"])
        self.verify()

    def test_self_only_pages_and_queued_discovery_recheck_live_project_grant(self):
        ready = [self.create_item(f"Ready observation {index}") for index in range(3)]
        different = self.create_item("Assigned elsewhere", agent=self.other)
        blocked = self.create_item("Wait for another observation", blockers=[different])
        withdrawn = self.create_item("No longer assigned for action")
        self.call("WorkItem.Abandon", {**target(withdrawn), "reason": "withdraw acceptance request"})
        project = self.project_ref(self.project)
        query = {"project_id": self.project}
        expected = sorted(ready, key=lambda ref: ref["resource_id"])
        first = self.assigned(project, limit=1)
        row, = first["items"]
        self.assertEqual(row["reference"], expected[0], first)
        self.assertEqual(first["next_after_id"], row["reference"]["resource_id"], first)
        rest = self.assigned(project, limit=2, after_id=first["next_after_id"])
        self.assertEqual([row["reference"] for row in rest["items"]], expected[1:], rest)
        self.assertIsNone(rest["next_after_id"], rest)
        all_own = self.call(ASSIGNED, {**query, "include_blocked": True}, self.agent)
        self.assertCountEqual([row["reference"] for row in all_own["items"]], ready + [blocked])
        waiting, = [row for row in all_own["items"] if row["reference"] == blocked]
        self.assertEqual(waiting["value"]["blockers"], [{"id": different["resource_id"], "lifecycle": "Ready"}])
        self.assertEqual([row["reference"] for row in self.call(ASSIGNED, query, self.other)["items"]], [different])
        token_file = self.node.base / "assigned-contributor.token"
        with os.fdopen(os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            stream.write(self.agent["token"])
        cli = subprocess.check_output([
            self.node.binaries / "psiosctl", "--socket", self.node.socket, "--token-file", token_file,
            "work-item", "assigned",
        ], input=json.dumps({**query, "limit": 1}), text=True, timeout=20)
        self.assertEqual(json.loads(cli), first)
        self.denied("PermissionDenied", ASSIGNED, query, self.reviewer)
        for invalid in ({"limit": 0}, {"limit": 17}, {"after_id": ""}):
            self.denied("InvalidArgument", ASSIGNED, {**query, **invalid}, self.agent)
        self.denied("Conflict", ASSIGNED, {**query, "expected_project_version": self.stale_project["version"]},
                    self.agent)

        reassigned = self.call("WorkItem.Reassign", {
            **target(expected.pop(0)), "assignee_principal_id": self.other["principal_id"],
        })["reference"]
        self.assertEqual([row["reference"] for row in self.call(ASSIGNED, query, self.agent)["items"]], expected)
        self.assertEqual(self.assigned(project, limit=2, after_id=first["next_after_id"]), rest)
        self.assertCountEqual([row["reference"] for row in self.call(ASSIGNED, query, self.other)["items"]],
                              [different, reassigned])
        self.denied("InvalidArgument", "Grant.Issue", {
            "principal_id": self.agent["principal_id"], "actions": [DISCOVER, "task.submit", "task.read"],
            "tool_targets": [self.profile["resource_id"]], "units": 8, "ttl_seconds": 300,
        })
        tool_grant = self.grant(self.agent, [DISCOVER, "task.submit", "task.read"], [self.project])
        delegation = tool_grant["delegation_ref"]["resource_id"]
        self.denied("InvalidArgument", "Task.Prepare", {
            "tool": DISCOVER, "input": query, "key": key(), "delegation_id": delegation,
        }, self.agent)
        self.schedule("Cordoned")
        stale_inputs = {**query, "expected_project_version": project["version"]}
        stale_request = self.node.prepare(DISCOVER, stale_inputs, token=self.agent["token"], delegation=delegation)
        stale = self.call("Task.Submit", stale_request, self.agent)["task_id"]
        self.assertEqual(self.call("Task.Get", {"id": stale})["lifecycle"], "Queued")
        current = self.member(self.project, self.reviewer, "Viewer")
        self.schedule("Enabled")
        stale_task = self.node.wait(stale)
        self.assertEqual((stale_task["lifecycle"], stale_task["outcome_knowledge"],
                          stale_task["conditions"][0]["type"]), ("Failed", "NotStarted", "Conflict"), stale_task)
        self.assertEqual(stale_task["result"]["artifact_refs"], [], stale_task)

        self.schedule("Cordoned")
        inputs = {**query, "expected_project_version": current["version"]}
        request = self.node.prepare(DISCOVER, inputs, token=self.agent["token"], delegation=delegation)
        admitted = self.call("Task.Submit", request, self.agent)
        task_id = admitted["task_id"]
        self.assertEqual(self.call("Task.Get", {"id": task_id})["lifecycle"], "Queued")
        self.call("Grant.Revoke", target(self.project_grant["grant_ref"]))
        self.denied("PermissionDenied", ASSIGNED, query, self.agent)
        self.restart()
        self.schedule("Enabled")
        failed = self.node.wait(task_id)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PermissionDenied"), failed)
        self.assertEqual(failed["result"]["artifact_refs"], [], failed)

        self.contribute(self.project, self.agent)
        replay = self.call("Task.Submit", request, self.agent)
        self.assertTrue(replay["deduplicated"] and replay["task_id"] == task_id, replay)
        self.assertEqual(self.call("Task.Get", {"id": task_id}), failed)
        fresh = self.node.prepare(DISCOVER, inputs, token=self.agent["token"], delegation=delegation)
        accepted = self.call("Task.Submit", fresh, self.agent)
        _, _, page = self.result(accepted["task_id"])
        self.assertEqual([row["reference"] for row in page["items"]], expected)
        self.assertEqual(self.call(ASSIGNED, query, self.agent), page)
        self.verify()


if __name__ == "__main__":
    unittest.main()
