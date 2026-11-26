"""Keep detached Workflow launches and their active strict leases across format 114."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest

from support import key
from test_agent_session_inputs import SessionInputFixture
from test_schedule import timestamp


class WorkflowSessionJoinUpgrade(SessionInputFixture, unittest.TestCase):
    def step(self, run, name):
        item, = [item for item in run["state"]["steps"] if item["step_id"] == name]
        return item

    def wait_until(self, run_id, ready):
        limit = time.monotonic() + 12
        while True:
            run = self.n.call("Workflow.Get", {"id": run_id}, self.agent["token"])
            if ready(run):
                return run
            self.assertNotIn(run["state"]["lifecycle"], ("Blocked", "Failed", "Cancelled", "Succeeded"), run)
            self.assertLess(time.monotonic(), limit, run)
            time.sleep(0.04)

    def definition(self, scope, completion=None):
        opening = self.opening(scope)
        launch = {"id": "launch", "tool": "agent.session.start", "after": [], "input": {
            field: opening[field] for field in ("goal", "skill_ref", "model_ref", "context_policy",
                                               "max_turns", "input_artifacts")}}
        if completion is not None:
            launch["completion"] = completion
        return {"key": key(), "steps": [launch, {"id": "observe", "tool": "system.status", "input": {},
                                                 "after": ["launch"]}]}

    def start(self, scope, definition):
        request = {"key": key(), "delegation_id": scope["delegation_ref"]["resource_id"],
                   "definition_ref": definition["reference"], "deadline_at": timestamp(90)}
        run = self.n.call("Workflow.Start", request, self.agent["token"])
        return request, run["reference"]["resource_id"]

    def step_output(self, run, name):
        task_id = self.step(run, name)["task_receipt"]["task_id"]
        task = self.n.call("Task.Get", {"id": task_id}, self.agent["token"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        self.assertEqual(task["origin_ref"]["origin_id"], run["reference"]["resource_id"])
        artifact, = task["result"]["artifact_refs"]
        return task, self.n.call("Artifact.Read", {"reference": artifact}, self.agent["token"])

    def task_ids(self):
        return {item["resource_id"] for item in self.n.call("Resource.List", {"kind": "Task"})["items"]}

    def verify(self, command):
        return json.loads(subprocess.check_output(command + ["verify"], timeout=20))

    def test_previous_launch_only_workflow_and_active_lease_survive_upgrade(self):
        n = self.n
        caller = self.agent["token"]
        scope = self.grant(self.agent, ["workflow.manage", "agent.session.start", "session.run",
            "session.context.read", "task.submit", "task.read", "system.status", "node.inventory", "artifact.read"],
            [self.node_id, "profile-local", self.model["resource_id"], self.skill["resource_id"],
             self.artifact["resource_id"]])
        legacy_request = self.definition(scope)
        legacy_def = n.call("Workflow.Define", legacy_request, caller)
        self.assertEqual(legacy_def["definition"]["dsl_version"], 1)
        self.refuse("InvalidArgument", "Workflow.Define", self.definition(scope, "SessionTerminal"), caller)
        request, run_id = self.start(scope, legacy_def)
        legacy = self.wait_until(run_id, lambda run: run["state"]["lifecycle"] == "Succeeded")
        old_task, launched = self.step_output(legacy, "launch")
        old_status, observed = self.step_output(legacy, "observe")
        self.assertTrue(observed["authority_id"])
        session = launched["session_ref"]
        self.assertEqual(self.session(session)["session"]["lifecycle"], "AwaitingModel")
        self.assertNotIn("session_ref", self.step(legacy, "launch"))
        claim_key = "before-workflow-session-join"
        issued = self.queued(claim_key, accept=True)
        self.assertEqual(issued["session_id"], session["resource_id"])
        self.assertEqual(issued["context"]["inputs"][0]["value"], self.data)
        old_ids = {old_task["task_id"], old_status["task_id"]}
        self.assertEqual(self.task_ids(), old_ids)

        previous = n.daemon[:]
        n.stop(signal.SIGKILL)
        old_report = self.verify(previous)
        self.assertEqual(old_report["database_format"], 114, old_report)
        n.daemon[0] = str(Path(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug")).absolute() / "psiosd")
        self.assertTrue(self.verify(n.daemon)["migration_required"])
        n.start()
        self.assertEqual(n.call("Workflow.Define", legacy_request, caller), legacy_def)
        self.assertEqual(n.call("Workflow.Start", request, caller), legacy)
        self.assertEqual(self.step_output(legacy, "launch"), (old_task, launched))
        self.assertEqual(self.step_output(legacy, "observe"), (old_status, observed))
        self.assertEqual(self.queued(claim_key, accept=True), {**issued, "recovered": True})
        self.assertEqual(self.model_calls(scope), 1)
        decision = {"action": "finish", "summary": "Finished the original format 114 lease."}
        settled = self.respond(session, issued, decision)
        self.assertEqual(settled["lifecycle"], "Succeeded")
        self.assertEqual(self.respond(session, issued, decision), settled)
        self.assertEqual(self.queued(claim_key, accept=True)["completed"], settled)
        self.assertEqual(n.call("Workflow.Get", {"id": run_id}, caller), legacy)
        self.assertEqual(self.task_ids(), old_ids)

        new_def = n.call("Workflow.Define", self.definition(scope, "SessionTerminal"), caller)
        self.assertEqual(new_def["definition"]["dsl_version"], 4)
        joined_request, joined_id = self.start(scope, new_def)
        waiting = self.wait_until(joined_id, lambda run: self.step(run, "launch").get("session_ref") is not None)
        new_task, new_launch = self.step_output(waiting, "launch")
        joined_session = new_launch["session_ref"]
        self.assertNotEqual(joined_session["resource_id"], session["resource_id"])
        self.assertEqual(self.step(waiting, "launch")["session_ref"], joined_session)
        self.assertIsNone(self.step(waiting, "observe")["task_receipt"])
        self.assertEqual(self.task_ids(), old_ids | {new_task["task_id"]})
        claim = self.queued("after-workflow-session-join", accept=True)
        self.assertEqual(claim["session_id"], joined_session["resource_id"], claim)
        self.assertEqual(claim["context"]["inputs"][0]["value"], self.data)
        final = self.respond(joined_session, claim, {"action": "finish", "summary": "Finished an attached lease."})
        done = self.wait_until(joined_id, lambda run: run["state"]["lifecycle"] == "Succeeded")
        self.assertEqual(self.step(done, "launch")["session_ref"], final["session_ref"])
        new_status, output = self.step_output(done, "observe")
        self.assertEqual(output["authority_id"], observed["authority_id"])
        self.assertEqual(self.task_ids(), old_ids | {new_task["task_id"], new_status["task_id"]})
        self.assertEqual(self.model_calls(scope), 2)
        self.assertEqual(n.call("Workflow.Start", joined_request, caller), done)
        self.assertEqual(self.queued(claim_key, accept=True)["completed"], settled)

        n.stop()
        new_report = self.verify(n.daemon)
        self.assertGreaterEqual(new_report["database_format"], 115, new_report)
        self.assertFalse(new_report["migration_required"], new_report)
        self.assertEqual(new_report["node_id"], old_report["node_id"])
        old_writer = subprocess.run(previous + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(old_writer.returncode, 0, old_writer)
        self.assertIn("SchemaVersionUnsupported", old_writer.stderr)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True)
    WorkflowSessionJoinUpgrade.bin_dir = parser.parse_args().previous_bin_dir
    unittest.main(argv=[__file__])
