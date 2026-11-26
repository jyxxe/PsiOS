"""Join a real strict Agent Session before releasing a Workflow dependency."""

import json
import signal
import subprocess
import time
import unittest

from support import key, target
from test_agent_session_inputs import SessionInputFixture
from test_schedule import timestamp


class WorkflowSessionJoin(SessionInputFixture, unittest.TestCase):
    def setUp(self):
        super().setUp()
        self.caller = self.agent["token"]
        creation = self.grant(self.agent, ["volume.create", "task.submit", "task.read"], None)
        self.child_volume = self.create_volume("agent-write", creation)
        self.follow_volume = self.create_volume("workflow-write", creation)
        self.child_path = self.volume_path(self.child_volume)
        self.follow_path = self.volume_path(self.follow_volume)
        self.skill = self.n.call("Skill.Publish", {"name": "workflow-session-write", "version": "1",
            "license": "0BSD", "source": "local-test", "tools": ["volume.write"],
            "instructions": "Request the volume write described by the fixed input."})
        self.data = {"name": "proof", "content": "Written by the Agent Session."}
        self.artifact = self.upload(json.dumps(self.data).encode(), self.agent)
        self.scope = self.grant(self.agent, [
            "workflow.manage", "agent.session.start", "session.run", "session.context.read",
            "task.submit", "task.read", "artifact.read", "volume.write",
        ], [self.node_id, self.model["resource_id"], self.skill["resource_id"], self.artifact["resource_id"],
            self.child_volume["resource_id"], self.follow_volume["resource_id"]])
        self.initial_tasks = self.task_ids()

    def create_volume(self, name, grant):
        request = self.n.prepare("volume.create", {"name": name}, token=self.caller,
                                 delegation=grant["delegation_ref"]["resource_id"])
        task = self.n.wait(self.n.call("Task.Submit", request, self.caller)["task_id"], autoapprove=True)
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        reference, = task["result"]["artifact_refs"]
        return self.n.call("Artifact.Read", {"reference": reference}, self.caller)

    def volume_path(self, volume):
        return self.n.state / "volumes" / volume["resource_id"] / "data" / "proof"

    def task_ids(self):
        return {ref["resource_id"] for ref in self.n.call("Resource.List", {"kind": "Task"})["items"]}

    def definition(self):
        opening = self.opening(self.scope)
        blueprint = {field: opening[field] for field in [
            "goal", "skill_ref", "model_ref", "context_policy", "max_turns", "input_artifacts",
        ]}
        return {"key": key(), "steps": [
            {"id": "launch", "tool": "agent.session.start", "input": blueprint, "after": [],
             "completion": "SessionTerminal"},
            {"id": "follow", "tool": "volume.write", "after": ["launch"],
             "input": {**target(self.follow_volume), "name": "proof", "content": "Written by the Workflow."}},
        ]}

    def start(self, deadline=60):
        definition = self.n.call("Workflow.Define", self.definition(), self.caller)["reference"]
        request = {"key": key(), "definition_ref": definition, "deadline_at": timestamp(deadline),
                   "delegation_id": self.scope["delegation_ref"]["resource_id"]}
        run = self.n.call("Workflow.Start", request, self.caller)
        return request, run["reference"]["resource_id"]

    def workflow(self, run_id):
        return self.n.call("Workflow.Get", {"id": run_id}, self.caller)

    def wait_for(self, run_id, predicate):
        deadline = time.monotonic() + 10
        while True:
            run = self.workflow(run_id)
            if predicate(run["state"]):
                return run
            self.assertNotIn(run["state"]["lifecycle"], ("Failed", "Cancelled", "Succeeded"), run)
            self.assertLess(time.monotonic(), deadline, run)
            time.sleep(0.04)

    def step(self, state, name):
        item, = [item for item in state["steps"] if item["step_id"] == name]
        return item

    def launch_receipt(self, run_id):
        run = self.wait_for(run_id, lambda state: self.step(state, "launch")["task_receipt"] is not None)
        step = self.step(run["state"], "launch")
        task = self.n.wait(step["task_receipt"]["task_id"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        self.assertEqual((task["origin_ref"]["origin_type"], task["origin_ref"]["origin_id"]),
                         ("WorkflowRun", run_id))
        proof, = task["result"]["artifact_refs"]
        session = self.n.call("Artifact.Read", {"reference": proof}, self.caller)["session_ref"]
        self.assertEqual(self.session(session)["session"]["origin_task_ref"]["resource_id"], task["task_id"])
        return task, session

    def wait_joined(self, run_id, session):
        run = self.wait_for(run_id, lambda state: self.step(state, "launch").get("session_ref") is not None)
        state = run["state"]
        launch = self.step(state, "launch")
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"]), ("Running", "InFlight"), run)
        self.assertEqual((launch["lifecycle"], launch["outcome_knowledge"]), ("Running", "InFlight"), launch)
        self.assertEqual(launch["session_ref"], session)
        self.assertEqual(launch["session_lifecycle"], "AwaitingModel")
        return run

    def pending(self, run_id, launch, *other_tasks):
        run = self.workflow(run_id)
        self.assertEqual(self.step(run["state"], "launch")["task_receipt"]["task_id"], launch)
        self.assertIsNone(self.step(run["state"], "follow")["task_receipt"], run)
        self.assertFalse(self.follow_path.exists())
        self.assertEqual(self.task_ids(), self.initial_tasks | {launch, *other_tasks})
        return run

    def claim_exact_input(self, session, claim_key):
        claimed = self.queued(claim_key, accept=True, wait_ms=1000)
        self.assertEqual(claimed["session_id"], session["resource_id"], claimed)
        self.assertEqual(claimed["inference_outcome_policy"], "Strict")
        self.assertEqual(claimed["context"]["inputs"][0]["value"], self.data)
        return claimed

    def verify(self):
        subprocess.run(self.n.daemon + ["verify"], check=True, capture_output=True, timeout=20)

    def restart(self):
        self.n.stop(signal.SIGKILL)
        self.verify()
        self.n.start()

    def test_approval_and_crash_keep_the_workflow_waiting_until_the_session_finishes(self):
        request, run_id = self.start()
        launch, session = self.launch_receipt(run_id)
        launch_id = launch["task_id"]
        joined = self.wait_joined(run_id, session)
        joined_step = self.step(joined["state"], "launch")
        self.pending(run_id, launch_id)
        claim = self.claim_exact_input(session, "join-child")
        decision = {"action": "tool", "tool": "volume.write",
                    "input": {**target(self.child_volume), **claim["context"]["inputs"][0]["value"]}}
        child = self.respond(session, claim, decision)
        child_id = child["task_id"]
        waiting = self.n.wait(child_id)
        self.assertEqual((waiting["lifecycle"], waiting["outcome_knowledge"]), ("WaitingApproval", "NotStarted"))
        self.assertFalse(self.child_path.exists())
        self.pending(run_id, launch_id, child_id)
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), self.caller)

        self.restart()
        self.pending(run_id, launch_id, child_id)
        self.assertEqual(self.n.call("Task.Get", {"id": child_id}, self.caller), waiting)
        self.assertEqual(self.respond(session, claim, decision), child)
        self.assertEqual(self.queued("no-turn-before-approval", accept=True), {"available": False})
        self.n.approve(child_id)
        self.assertEqual(self.n.wait(child_id)["lifecycle"], "Succeeded")
        self.assertEqual(self.child_path.read_text(encoding="utf-8"), self.data["content"])
        self.pending(run_id, launch_id, child_id)
        self.assertEqual(self.n.call("Task.Get", {"id": launch_id}, self.caller), launch)

        last = self.claim_exact_input(session, "join-finish")
        self.assertEqual(last["context"]["last_tool"]["task_id"], child_id)
        finish = {"action": "finish", "summary": "Approved child write completed."}
        settled = self.respond(session, last, finish)
        self.assertEqual(settled["lifecycle"], "Succeeded")
        waiting = self.wait_for(run_id, lambda state: self.step(state, "follow")["lifecycle"] == "WaitingApproval")
        follow = self.step(waiting["state"], "follow")["task_receipt"]["task_id"]
        self.assertFalse(self.follow_path.exists())
        self.n.approve(follow)
        final = self.wait_for(run_id, lambda state: state["lifecycle"] == "Succeeded")
        self.assertEqual(final["state"]["outcome_knowledge"], "Known")
        finished = self.step(final["state"], "launch")
        self.assertEqual((finished["lifecycle"], finished["outcome_knowledge"]), ("Succeeded", "Known"))
        self.assertEqual(finished["task_ref"], joined_step["task_ref"])
        self.assertEqual(finished["task_receipt"], joined_step["task_receipt"])
        self.assertEqual(finished["session_ref"], self.session(session)["reference"])
        self.assertEqual(finished["session_lifecycle"], "Succeeded")
        follow_step = self.step(final["state"], "follow")
        self.assertEqual(follow_step["lifecycle"], "Succeeded")
        self.assertEqual(follow_step["task_receipt"]["origin_ref"]["origin_id"], run_id)
        self.assertEqual(self.follow_path.read_text(encoding="utf-8"), "Written by the Workflow.")
        self.assertEqual(self.task_ids(), self.initial_tasks | {launch_id, child_id, follow})
        self.assertEqual(self.model_calls(self.scope), 2)
        self.assertEqual(self.respond(session, last, finish), settled)
        self.assertEqual(self.n.call("Workflow.Start", request, self.caller), final)
        self.n.call("Artifact.Withdraw", self.withdrawal(), self.caller)
        self.n.stop()
        self.verify()

    def test_cancel_joins_original_session_without_admitting_successor(self):
        request, run_id = self.start()
        launch, session = self.launch_receipt(run_id)
        self.wait_joined(run_id, session)
        self.restart()
        prior = self.pending(run_id, launch["task_id"])
        self.n.call("Workflow.Cancel", target(prior["reference"]), self.caller)
        cancelled = self.wait_for(run_id, lambda state: state["lifecycle"] == "Cancelled")
        self.assertEqual(cancelled["state"]["outcome_knowledge"], "Known")
        stopped = self.session(session)
        self.assertEqual(stopped["session"]["lifecycle"], "Cancelled")
        step = self.step(cancelled["state"], "launch")
        self.assertEqual((step["session_ref"], step["session_lifecycle"]), (stopped["reference"], "Cancelled"))
        self.assertEqual(self.queued("no-turn-after-cancel", accept=True), {"available": False})
        self.assertEqual(self.pending(run_id, launch["task_id"]), cancelled)
        self.assertEqual(self.n.call("Task.Get", {"id": launch["task_id"]}, self.caller), launch)
        self.assertEqual(self.n.call("Workflow.Start", request, self.caller), cancelled)
        self.n.call("Artifact.Withdraw", self.withdrawal(), self.caller)
        self.n.stop()
        self.verify()

    def test_deadline_cancels_the_exact_already_launched_session(self):
        _, run_id = self.start(deadline=4)
        launch, session = self.launch_receipt(run_id)
        self.wait_joined(run_id, session)
        expired = self.wait_for(run_id, lambda state: state["lifecycle"] == "Failed")
        self.assertEqual((expired["state"]["reason"], expired["state"]["outcome_knowledge"]),
                         ("DeadlineExceeded", "Known"), expired)
        stopped = self.session(session)
        self.assertEqual(stopped["session"]["lifecycle"], "Cancelled")
        self.assertEqual(self.step(expired["state"], "launch")["session_ref"], stopped["reference"])
        self.assertEqual(self.queued("no-turn-after-deadline", accept=True), {"available": False})
        self.assertEqual(self.pending(run_id, launch["task_id"]), expired)
        self.assertEqual(self.n.call("Task.Get", {"id": launch["task_id"]}, self.caller), launch)
        self.n.stop()
        self.verify()

    def test_unknown_session_fences_successor_even_after_workflow_cancellation(self):
        _, run_id = self.start()
        launch, session = self.launch_receipt(run_id)
        self.wait_joined(run_id, session)
        first = self.claim_exact_input(session, "join-unknown")
        failure = {"session_id": session["resource_id"], "lease_id": first["lease_id"],
                   "model_ref": self.model, "outcome": "Unknown"}
        receipt = self.n.call("Model.ReportFailure", failure, self.provider["token"])
        self.assertEqual((receipt["lifecycle"], receipt["outcome_knowledge"]), ("Paused", "Unknown"))
        blocked = self.wait_for(run_id, lambda state: state["lifecycle"] == "Blocked")
        self.assert_unknown(blocked, receipt["session_ref"], "Paused")
        self.pending(run_id, launch["task_id"])

        self.restart()
        self.assertEqual(self.pending(run_id, launch["task_id"]), blocked)
        self.assertEqual(self.n.call("Model.ReportFailure", failure, self.provider["token"]), receipt)
        self.assertEqual(self.queued("no-new-inference-after-unknown", accept=True), {"available": False})
        self.n.call("Workflow.Cancel", target(blocked["reference"]), self.caller)
        stopping = self.wait_for(run_id, lambda state: self.step(state, "launch")["session_lifecycle"] == "Cancelled")
        stopped = self.session(session)
        self.assertEqual(stopped["session"]["outcome_knowledge"], "Unknown")
        self.assert_unknown(stopping, stopped["reference"], "Cancelled")
        self.assertEqual(self.pending(run_id, launch["task_id"]), stopping)
        self.assertEqual(self.model_calls(self.scope), 1)
        self.n.stop()
        self.verify()

    def assert_unknown(self, run, session_ref, session_lifecycle):
        state = run["state"]
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"], state["reason"]),
                         ("Blocked", "Unknown", "OutcomeUnknown"), run)
        step = self.step(state, "launch")
        self.assertEqual((step["outcome_knowledge"], step["reason"]), ("Unknown", "OutcomeUnknown"), step)
        self.assertEqual((step["session_ref"], step["session_lifecycle"]), (session_ref, session_lifecycle), step)
