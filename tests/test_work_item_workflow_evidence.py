"""Bind WorkItem acceptance to an exact settled Workflow step and its Task output."""
import json
import os
import signal
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from test_schedule import timestamp


class WorkItemWorkflowEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.contributor = self.call("Agent.Register", {"name": "workflow-evidence-contributor"})
        self.reviewer = self.call("Agent.Register", {"name": "workflow-evidence-reviewer"})
        self.project = self.call("Project.Create", {"key": key(), "title": "Verify an exact workflow observation"})
        self.project_id = self.project["reference"]["resource_id"]
        self.profile = self.node.get("profile-local")["reference"]
        self.grant("local-owner", ["project.read", "project.manage"], [self.project_id])
        for actor, role, action in (
            (self.contributor, "Contributor", "project.contribute"),
            (self.reviewer, "Reviewer", "project.review"),
        ):
            self.project = self.call("Project.SetMember", {
                **target(self.project["reference"]), "principal_id": actor["principal_id"], "role": role,
            })
            self.grant(actor["principal_id"], ["project.read", action], [self.project_id])
        self.workflow_grant = self.grant(
            self.contributor["principal_id"],
            ["workflow.manage", "resource.read", "task.submit", "task.read", "system.status"],
            [self.profile["resource_id"]],
        )
        self.grant(self.reviewer["principal_id"], ["workflow.manage", "resource.read", "task.read"],
                   [self.profile["resource_id"]])

    def call(self, method, params, actor=None):
        return self.node.call(method, params, actor["token"] if actor else None)

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.call(method, params, actor)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def grant(self, principal, actions, targets):
        grant = self.call("Grant.Issue", {
            "principal_id": principal, "actions": actions, "tool_targets": targets,
            "units": 12, "ttl_seconds": 300,
        })
        return grant["delegation_ref"]["resource_id"]

    def define(self, steps, actor=None):
        request = {
            "key": key(),
            "steps": [{"id": step, "tool": "system.status", "input": {}, "after": list(after)}
                      for step, after in steps],
        }
        return self.call("Workflow.Define", request, actor or self.contributor)["reference"]

    def run_workflow(self, definition):
        run = self.call("Workflow.Start", {
            "key": key(), "definition_ref": definition, "delegation_id": self.workflow_grant,
            "deadline_at": timestamp(90),
        }, self.contributor)
        run_id = run["reference"]["resource_id"]
        deadline = time.monotonic() + 30
        while True:
            run = self.call("Workflow.Get", {"id": run_id}, self.contributor)
            lifecycle = run["state"]["lifecycle"]
            if lifecycle == "Succeeded":
                self.assertEqual(run["state"]["outcome_knowledge"], "Known", run)
                self.assertEqual(run["definition_ref"], definition, run)
                return run
            self.assertNotIn(lifecycle, ("Failed", "Cancelled", "Blocked"), run)
            self.assertLess(time.monotonic(), deadline, run)
            time.sleep(0.04)

    def output(self, task_id):
        task = self.call("Task.Get", {"id": task_id}, self.contributor)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        return self.node.get(task_id)["reference"], artifact

    def step_output(self, run, step_id):
        entry, = [entry for entry in run["state"]["steps"] if entry["step_id"] == step_id]
        self.assertEqual((entry["lifecycle"], entry["outcome_knowledge"]), ("Succeeded", "Known"), entry)
        task, artifact = self.output(entry["task_receipt"]["task_id"])
        self.assertEqual(entry["task_ref"], task, entry)
        return task, artifact, entry["step_key"]

    def run_task(self, tool, inputs, delegation):
        request = self.node.prepare(tool, inputs, token=self.contributor["token"], delegation=delegation)
        task_id = self.call("Task.Submit", request, self.contributor)["task_id"]
        self.node.wait(task_id)
        return self.output(task_id)

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def assert_no_workflow_refs(self, view, definition, run):
        payload = json.dumps(view)
        self.assertNotIn(definition["resource_id"], payload)
        self.assertNotIn(run["reference"]["resource_id"], payload)
        self.assertNotIn("workflow_witness", payload)
        self.assertNotIn("step_id", payload)
        self.assertNotIn("kernel_release", payload)

    def test_exact_workflow_origin_and_independently_scoped_review_survive_crash(self):
        definition = self.define([("comparison", ()), ("observe", ("comparison",))])
        different_definition = self.define([("observe", ())])
        reviewer_definition = self.define([("observe", ())], self.reviewer)
        criterion = {
            "kind": "WorkflowTaskResult", "definition_ref": definition, "step_id": "observe",
            "tool": "system.status", "target_ref": self.profile,
        }
        creation = {
            "project_id": self.project_id, "expected_project_version": self.project["reference"]["version"],
            "key": key(), "title": "Review the nominated workflow step",
            "assignee_principal_id": self.contributor["principal_id"], "criterion": criterion,
        }
        for code, override in (
            ("InvalidArgument", {"step_id": "missing"}),
            ("PreconditionFailed", {"definition_ref": {**definition, "digest": "sha256:" + "0" * 64}}),
            ("PermissionDenied", {"definition_ref": reviewer_definition}),
        ):
            self.denied(code, "WorkItem.Create", {**creation, "criterion": {**criterion, **override}})
        created = self.call("WorkItem.Create", creation)
        item = created["reference"]
        where = {"id": item["resource_id"]}
        self.assertEqual(created["value"]["criterion"], criterion, created)
        self.assertEqual(created["value"]["lifecycle"], "Ready", created)

        manager = self.call("Agent.Register", {"name": "project-manager-without-workflow"})
        manager_id = manager["principal_id"]
        current = self.call("Project.SetMember", {
            **target(self.project["reference"]), "principal_id": manager_id, "role": "Manager",
        })
        self.grant(manager_id, ["project.read", "project.manage"], [self.project_id])
        self.grant(manager_id, ["task.read"], [self.profile["resource_id"]])
        for definition_ref in (definition, {**definition, "resource_id": "missing-workflow"}):
            self.denied("PermissionDenied", "WorkItem.Create", {
                **creation, "key": key(), "expected_project_version": current["reference"]["version"],
                "criterion": {**criterion, "definition_ref": definition_ref},
            }, manager)
        self.call("Project.SetMember", {
            **target(current["reference"]), "principal_id": manager_id, "role": "Contributor",
        })
        self.grant(manager_id, ["project.read", "project.contribute"], [self.project_id])
        self.denied("PermissionDenied", "WorkItem.Reassign", {
            **target(item), "assignee_principal_id": manager_id,
        })

        workflow = self.run_workflow(definition)
        expected_task, output, step_key = self.step_output(workflow, "observe")
        wrong_step, wrong_step_output, _ = self.step_output(workflow, "comparison")
        other_workflow = self.run_workflow(different_definition)
        wrong_run, wrong_run_output, _ = self.step_output(other_workflow, "observe")
        ordinary_task, ordinary_output = self.run_task("system.status", {}, self.workflow_grant)
        self.assertEqual(len({entry["resource_id"] for entry in (expected_task, wrong_step, wrong_run, ordinary_task)}), 4)
        payload = self.call("Artifact.Read", {"reference": output}, self.contributor)
        self.assertTrue(payload["authority_id"] and payload["kernel_release"], payload)

        for source, artifact in (
            (ordinary_task, ordinary_output),
            (wrong_step, wrong_step_output),
            (wrong_run, wrong_run_output),
            (expected_task, wrong_step_output),
            (expected_task, {**output, "digest": "sha256:" + "0" * 64}),
        ):
            with self.subTest(task=source["resource_id"], artifact=artifact["resource_id"]):
                self.denied("PreconditionFailed", "WorkItem.SubmitEvidence", {
                    **target(item), "task_ref": source, "artifact_ref": artifact,
                }, self.contributor)
                self.assertEqual(self.call("WorkItem.Get", where), created)

        for method, reference in (("Workflow.DefinitionGet", definition), ("Workflow.Get", workflow["reference"])):
            self.denied("PermissionDenied", method, {"id": reference["resource_id"]}, self.reviewer)
        self.denied("PermissionDenied", "Artifact.Read", {"reference": output}, self.reviewer)
        self.assert_no_workflow_refs(self.call("WorkItem.Get", where, self.reviewer), definition, workflow)
        self.assert_no_workflow_refs(self.call("WorkItem.List", {"project_id": self.project_id}, self.reviewer),
                                     definition, workflow)

        tool = "project.submit_evidence"
        execution = self.grant(self.contributor["principal_id"], ["task.submit", "task.read", tool],
                               [item["resource_id"]])
        receipt_task, receipt_artifact = self.run_task(tool, {
            **target(item), "task_ref": expected_task, "artifact_ref": output,
        }, execution)
        self.assertNotEqual(receipt_task, expected_task)
        pending = self.call("WorkItem.Get", where, self.contributor)
        receipt = self.call("Artifact.Read", {"reference": receipt_artifact}, self.contributor)
        self.assertEqual(receipt, {"work_item_ref": pending["reference"], "lifecycle": "InReview"})
        evidence = pending["value"]["evidence"]
        self.assertEqual((evidence["task_ref"], evidence["artifact_ref"]), (expected_task, output), evidence)
        self.assertEqual(evidence["criterion"], criterion)
        witness = evidence["workflow_witness"]
        self.assertEqual((witness["step_id"], witness["step_key"], witness["task_ref"]),
                         ("observe", step_key, expected_task), witness)
        self.assertEqual(witness["workflow_run_ref"]["resource_id"], workflow["reference"]["resource_id"], witness)
        view = self.call("Resource.Resolve", {"reference": witness["workflow_run_ref"]})["value"]
        settled, = [entry for entry in view["steps"] if entry["step_id"] == "observe"]
        self.assertEqual(settled["task_ref"], expected_task, settled)

        evidence_read = {"reference": output}
        review = {**target(pending["reference"]), "accept": True}
        eviction = {**target(expected_task), "reason": "release reviewed workflow observation"}
        self.denied("Conflict", "Task.EvictResults", eviction)
        self.denied("PermissionDenied", "WorkItem.Review", review, self.reviewer)
        self.grant(self.reviewer["principal_id"], ["task.results.read"], [expected_task["resource_id"]])
        self.assertEqual(self.call("Artifact.Read", evidence_read, self.reviewer), payload)
        for private in (wrong_step_output, receipt_artifact):
            self.denied("PermissionDenied", "Artifact.Read", {"reference": private}, self.reviewer)
        self.restart()
        self.assertEqual(self.call("Workflow.Get", {
            "id": workflow["reference"]["resource_id"],
        }, self.contributor), workflow)
        actual = self.call("WorkItem.Get", where, self.contributor)
        self.assertEqual(actual, pending)
        revision = self.call("WorkItem.RevisionGet", {"reference": pending["reference"]}, self.reviewer)
        shared = revision["value"]["evidence"]
        self.assertEqual((shared["task_ref"], shared["artifact_ref"]), (expected_task, output), shared)
        self.assert_no_workflow_refs(revision, definition, workflow)
        accepted = self.call("WorkItem.Review", review, self.reviewer)
        self.assertEqual(accepted["value"]["lifecycle"], "Verified", accepted)
        self.assertEqual(accepted["value"]["review"]["reviewer_principal_id"], self.reviewer["principal_id"])
        self.assert_no_workflow_refs(accepted, definition, workflow)
        self.denied("Conflict", "Task.EvictResults", eviction)

        closed = self.call("WorkItem.Close", target(accepted["reference"]))
        self.assertEqual(closed["value"]["lifecycle"], "Closed", closed)
        self.call("Task.EvictResults", eviction)
        self.denied("ResultEvicted", "Artifact.Read", evidence_read, self.reviewer)
        self.restart()
        self.assertEqual(self.call("WorkItem.Get", where), closed)
        self.assertEqual(self.call("WorkItem.RevisionGet", {"reference": pending["reference"]}, self.reviewer),
                         revision)
        self.verify()


if __name__ == "__main__":
    unittest.main()
