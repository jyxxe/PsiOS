"""A Workflow step can launch a session with an exact, literal private input."""

import signal
import subprocess
import time
import unittest

from support import key
from test_agent_session_inputs import SessionInputFixture
from test_schedule import timestamp


class WorkflowBrokeredSessionInput(SessionInputFixture, unittest.TestCase):
    def test_fixed_workflow_input_retains_normal_task_origin_and_provider_scope(self):
        n = self.n
        token = self.agent["token"]
        grant = self.grant(self.agent, [
            "workflow.manage", "agent.session.start", "session.run", "session.context.read",
            "task.submit", "task.read", "system.status", "node.inventory", "artifact.read",
        ], [self.node_id, "profile-local", self.model["resource_id"], self.skill["resource_id"],
            self.artifact["resource_id"]])
        opening = self.opening(grant)
        blueprint = {field: opening[field] for field in [
            "goal", "skill_ref", "model_ref", "context_policy", "max_turns", "input_artifacts",
        ]}
        step = {"id": "launch", "tool": "agent.session.start", "input": blueprint, "after": ["status"]}
        previous_step = {"id": "status", "tool": "system.status", "input": {}, "after": []}
        self.refuse("InvalidArgument", "Workflow.Define", {"key": key(), "steps": [
            previous_step,
            {**step, "inputs_from": {"input_artifacts": {"step": "status", "pointer": "/artifact"}}},
        ]}, token)
        definition = n.call("Workflow.Define", {"key": key(), "steps": [previous_step, step]}, token)["reference"]
        run = n.call("Workflow.Start", {"key": key(), "definition_ref": definition, "deadline_at": timestamp(90),
                     "delegation_id": grant["delegation_ref"]["resource_id"]}, token)
        run_id = run["reference"]["resource_id"]
        timeout = time.monotonic() + 30
        while run["state"]["lifecycle"] != "Succeeded":
            self.assertNotIn(run["state"]["lifecycle"], ("Failed", "Blocked", "Cancelled"), run)
            self.assertLess(time.monotonic(), timeout, run)
            time.sleep(0.04)
            run = n.call("Workflow.Get", {"id": run_id}, token)
        launch, = [status for status in run["state"]["steps"] if status["step_id"] == "launch"]
        task = n.call("Task.Get", {"id": launch["task_receipt"]["task_id"]}, token)
        self.assertEqual(task["origin_ref"]["origin_type"], "WorkflowRun")
        self.assertEqual(task["origin_ref"]["origin_id"], run_id)
        receipt = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)
        session = receipt["session_ref"]
        self.assertEqual(self.session(session)["session"]["origin_task_ref"]["resource_id"], task["task_id"])
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), token)

        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
        n.start()
        self.assertEqual(self.queued("legacy-workflow-worker"), {"available": False})
        claim = self.queued("workflow-literal", accept=True, wait_ms=1000)
        self.assertEqual(claim["session_id"], session["resource_id"])
        self.assertEqual(claim["context"]["inputs"][0]["value"], self.data)
        self.respond(session, claim, {"action": "finish", "summary": "Fixed input consumed."})
        n.call("Artifact.Withdraw", self.withdrawal(), token)
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
