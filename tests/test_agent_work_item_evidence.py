"""Exercise the Agent-to-WorkItem evidence path using native Unix RPC."""
import json
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key, target


TOOL = "project.submit_evidence"


class AgentWorkItemEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.agent = self.node.call("Agent.Register", {"name": "work-item-tool-contributor"})
        self.provider = self.node.call("Agent.Register", {"name": "work-item-tool-model"})
        self.reviewer = self.node.call("Agent.Register", {"name": "work-item-tool-reviewer"})
        self.model = self.node.call("Model.Register", {
            "name": "work-item-observer", "revision": "1", "provider_principal_id": self.provider["principal_id"],
            "lease_seconds": 60, "max_context_bytes": 32_768, "max_output_bytes": 8192, "max_in_flight": 1,
            "inference_outcome_policy": "Strict",
        })
        self.grant(self.provider, ["model.serve"], [self.model["resource_id"]])
        self.skill = self.node.call("Skill.Publish", {
            "name": "supply-work-item-evidence", "version": "1", "source": "local-test", "license": "0BSD",
            "instructions": "Observe the node and submit its exact Task result to the assigned work item.",
            "tools": ["system.status", TOOL],
        })
        project = self.node.call("Project.Create", {"key": key(), "title": "Local status evidence"})
        self.project_id = project["reference"]["resource_id"]
        self.profile = self.node.get("profile-local")["reference"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [self.project_id])
        self.member(self.agent, "Contributor")
        self.member(self.reviewer, "Reviewer")
        self.project_grant = self.contribute_grant()
        self.grant(self.reviewer, ["project.read", "project.review"], [self.project_id])

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor is not None else None)

    def grant(self, actor, actions, targets, **options):
        return self.node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions, "tool_targets": targets,
            "units": 8, "ttl_seconds": 240, **options,
        })

    def contribute_grant(self):
        return self.grant(self.agent, ["project.read", "project.contribute"], [self.project_id])

    def member(self, actor, role):
        project = self.node.call("Project.Get", {"id": self.project_id})
        self.node.call("Project.SetMember", {
            **target(project["reference"]), "principal_id": actor["principal_id"], "role": role,
        })

    def create_item(self, title):
        project = self.node.call("Project.Get", {"id": self.project_id})
        return self.node.call("WorkItem.Create", {
            "project_id": self.project_id, "expected_project_version": project["reference"]["version"],
            "key": key(), "title": title, "assignee_principal_id": self.agent["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
        })["reference"]

    def open_session(self, item):
        grant = self.grant(self.agent, ["session.run", "task.submit", "task.read", "system.status", TOOL],
                           [self.model["resource_id"], self.skill["resource_id"],
                            self.profile["resource_id"], item["resource_id"]], model_calls=4)
        session = self.call("Session.Open", {
            "key": key(), "goal": "Provide independent review with the node status.",
            "model_ref": self.model, "skill_ref": self.skill, "max_turns": 4,
            "provider_queue": True, "context_policy": "LocalToolResults", "tool_context_window": 2,
            "delegation_id": grant["delegation_ref"]["resource_id"],
        }, self.agent)["reference"]
        return session["resource_id"], grant["budget_ref"]["resource_id"]

    def claim(self, name):
        return self.call("Model.ClaimNext", {"model_ref": self.model, "key": name}, self.provider)

    def respond(self, request):
        return self.call("Model.Respond", request, self.provider)

    def decision(self, claim, tool, inputs):
        return {"session_id": claim["session_id"], "lease_id": claim["lease_id"],
                "decision": {"action": "tool", "tool": tool, "input": inputs}}

    def result(self, task_id):
        task = self.node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        payload = self.call("Artifact.Read", {"reference": artifact}, self.agent)
        return self.node.get(task_id)["reference"], artifact, payload

    def observe(self, session):
        claim = self.claim("status")
        self.assertEqual(claim["session_id"], session, claim)
        response = self.respond(self.decision(claim, "system.status", {}))
        task, artifact, payload = self.result(response["task_id"])
        self.assertTrue(payload["authority_id"], payload)
        return task, artifact, payload

    def submit_params(self, item, task, artifact):
        return {**target(item), "task_ref": task, "artifact_ref": artifact}

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as rejected:
            self.call(method, params, actor)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    def tasks(self):
        return {ref["resource_id"] for ref in self.node.call("Resource.List", {"kind": "Task"})["items"]}

    def budget(self, reference):
        budget = self.node.call("Budget.Get", {"id": reference})
        return budget["model_calls_used"], budget["spent"]

    def scheduling(self, mode):
        node = self.node.call("Node.Get", {})["reference"]
        self.node.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 82, report)
        self.assertFalse(report["migration_required"], report)

    def test_model_attaches_one_exact_result_then_independent_principal_reviews(self):
        n = self.node
        item = self.create_item("Record intended host status")
        other = self.create_item("Separate target with the same assignee")
        where = {"id": item["resource_id"]}
        session, budget = self.open_session(item)
        source, original_artifact, observation = self.observe(session)
        claim = self.claim("attach")
        self.assertEqual((claim["session_id"], claim["turn"]), (session, 2), claim)
        last = claim["context"]["last_tool"]
        self.assertEqual((last["task_ref"], last["result"]["artifact_refs"], last["local_tool_result"]),
                         (source, [original_artifact], observation), last)
        inputs = self.submit_params(item, last["task_ref"], last["result"]["artifact_refs"][0])
        response = self.decision(claim, TOOL, inputs)
        for code, invalid in (("PermissionDenied", self.submit_params(other, source, original_artifact)),
                              ("Conflict", {**inputs, "expected_resource_version": "r99"})):
            self.denied(code, "Model.Respond", self.decision(claim, TOOL, invalid), self.provider)

        # The Project Grant is independent from the still-current Session and serving leases.
        n.call("Grant.Revoke", target(self.project_grant["grant_ref"]))
        self.denied("PermissionDenied", "Model.Respond", response, self.provider)
        self.assertEqual(n.call("WorkItem.Get", where)["reference"], item)
        self.assertEqual(n.call("WorkItem.Get", {"id": other["resource_id"]})["reference"], other)
        self.assertEqual(self.tasks(), {source["resource_id"]})
        self.assertEqual(self.budget(budget), (2, 1))
        self.contribute_grant()

        self.scheduling("Cordoned")
        accepted = self.respond(response)
        self.assertEqual(accepted["lifecycle"], "WaitingTask", accepted)
        receipt_task = accepted["task_id"]
        self.assertNotEqual(receipt_task, source["resource_id"])
        queued = n.call("Task.Get", {"id": receipt_task})
        self.assertEqual((queued["lifecycle"], queued["outcome_knowledge"]), ("Queued", "NotStarted"), queued)
        operation, = queued["operation_refs"]
        plan_ref = n.call("Resource.Resolve", {"reference": operation})["value"]["plan_ref"]
        plan = n.call("Resource.Resolve", {"reference": plan_ref})["value"]
        self.assertEqual([effect["resource_ref"] for effect in plan["allowed_effects"]], [item], plan)
        self.assertEqual([condition["resource_ref"] for condition in plan["preconditions"]], [item], plan)
        self.assertEqual(n.call("WorkItem.Get", where)["reference"], item)

        self.restart()
        self.assertEqual(self.respond(response), accepted)
        self.assertEqual(self.claim("attach")["completed"], accepted)
        self.assertEqual(n.call("Task.Get", {"id": receipt_task}), queued)
        self.scheduling("Enabled")
        settled, receipt_artifact, receipt = self.result(receipt_task)
        attached = self.call("WorkItem.Get", where, self.agent)
        self.assertEqual(receipt, {"work_item_ref": attached["reference"], "lifecycle": "InReview"})
        saved = attached["value"]["evidence"]
        self.assertEqual((saved["task_ref"], saved["artifact_ref"], saved["submitted_by"]),
                         (source, original_artifact, self.agent["principal_id"]), saved)
        self.assertEqual(self.respond(response), accepted)
        self.assertEqual(self.call("WorkItem.Get", where, self.agent), attached)
        self.assertEqual(self.tasks(), {source["resource_id"], receipt_task})

        third = self.claim("finish")
        self.assertEqual(third["session_id"], session, third)
        last = third["context"]["last_tool"]
        self.assertEqual((last["task_ref"], last["local_tool_result"]), (settled, receipt), last)
        finished = self.respond({"session_id": session, "lease_id": third["lease_id"],
                                 "decision": {"action": "finish", "summary": "Submitted the exact status evidence."}})
        self.assertEqual(finished["lifecycle"], "Succeeded", finished)
        self.assertEqual(self.budget(budget), (3, 2))
        steps = self.call("Session.History", {"id": session}, self.agent)["tasks"]
        self.assertEqual([(step["turn"], step["task_id"]) for step in steps],
                         [(1, source["resource_id"]), (2, receipt_task)])

        review = {**target(attached["reference"]), "accept": True}
        self.denied("PermissionDenied", "WorkItem.Review", review, self.reviewer)
        self.grant(self.reviewer, ["task.results.read"], [source["resource_id"]])
        self.assertEqual(self.call("Artifact.Read", {"reference": original_artifact}, self.reviewer), observation)
        self.denied("PermissionDenied", "Artifact.Read", {"reference": receipt_artifact}, self.reviewer)
        reviewed = self.call("WorkItem.Review", review, self.reviewer)
        self.assertEqual(reviewed["value"]["lifecycle"], "Verified", reviewed)
        self.assertEqual(reviewed["value"]["review"]["reviewer_principal_id"], self.reviewer["principal_id"])
        self.assertEqual(reviewed["value"]["evidence"], saved)
        self.verify()

    def test_membership_removed_after_submission_fences_dispatch_after_restart(self):
        n = self.node
        item = self.create_item("Withdraw the contributor while queued")
        where = {"id": item["resource_id"]}
        session, budget = self.open_session(item)
        source, original_artifact, observation = self.observe(session)
        claim = self.claim("attach")
        self.scheduling("Cordoned")
        decision = self.decision(claim, TOOL, self.submit_params(item, source, original_artifact))
        accepted = self.respond(decision)
        task_id = accepted["task_id"]
        self.assertEqual(n.call("Task.Get", {"id": task_id})["lifecycle"], "Queued")
        self.member(self.agent, None)
        self.restart()
        self.scheduling("Enabled")
        failed = n.wait(task_id)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PermissionDenied"), failed)
        self.assertEqual(failed["result"]["artifact_refs"], [], failed)
        self.assertEqual(n.call("WorkItem.Get", where)["reference"], item)
        self.assertEqual(self.call("Artifact.Read", {"reference": original_artifact}, self.agent), observation)

        self.member(self.agent, "Contributor")
        self.assertEqual(self.respond(decision), accepted)
        self.assertEqual(n.call("Task.Get", {"id": task_id}), failed)
        self.assertEqual(n.call("WorkItem.Get", where)["reference"], item)
        self.assertEqual(self.tasks(), {source["resource_id"], task_id})
        self.assertEqual(self.budget(budget), (2, 1))
        self.verify()

    def test_wrong_artifact_digest_cannot_publish_an_evidence_receipt(self):
        n = self.node
        item = self.create_item("Require the exact status artifact")
        session, budget = self.open_session(item)
        source, artifact, observation = self.observe(session)
        claim = self.claim("attach")
        forged = {**artifact, "digest": "sha256:" + "0" * 64}
        request = self.decision(claim, TOOL, self.submit_params(item, source, forged))
        accepted = self.respond(request)
        task_id = accepted["task_id"]
        failed = n.wait(task_id)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PreconditionFailed"), failed)
        self.assertEqual(failed["result"]["artifact_refs"], [], failed)
        untouched = n.call("WorkItem.Get", {"id": item["resource_id"]})
        self.assertEqual(untouched["reference"], item)
        self.assertFalse(untouched["value"]["has_evidence"], untouched)
        self.assertEqual(self.call("Artifact.Read", {"reference": artifact}, self.agent), observation)
        self.assertEqual(self.respond(request), accepted)
        self.assertEqual(self.tasks(), {source["resource_id"], task_id})
        self.assertEqual(self.budget(budget), (2, 1))
        self.verify()


if __name__ == "__main__":
    unittest.main()
