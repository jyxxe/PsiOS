"""Exercise owner pause boundaries through real direct and queued Agent turns."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
import unittest

from support import Failure, Node, key, target
from test_artifact_ingress import chunk, upload_request


class AgentSafePointPauseTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.actor = self.n.call("Agent.Register", {"name": "pause-owner"})
        self.actor_token = self.actor["token"]
        self.actor_file = self.n.base / "pause-owner.token"
        self.actor_file.write_text(self.actor_token)
        self.actor_file.chmod(0o600)
        self.provider, _ = self.n.agent(["model.serve"])
        self.model = self.n.call("Model.Register", {
            "name": "safe-point-provider", "revision": key(), "provider_principal_id": self.provider["principal_id"],
            "inference_outcome_policy": "Strict", "lease_seconds": 60, "max_in_flight": 4,
            "max_context_bytes": 16384, "max_output_bytes": 2048,
        })
        self.skill = self.n.call("Skill.Publish", {
            "name": "safe-point-tools", "version": "1", "license": "0BSD", "source": "local-test",
            "tools": ["node.inventory", "volume.write"], "instructions": "Use the one tool requested by the owner.",
        })
        self.actions = ["session.run", "task.read", "task.submit", "node.inventory", "volume.write"]
        self.grant = self.issue(self.actions)

    def issue(self, actions, targets=None):
        scope = {} if targets is None else {"tool_targets": targets}
        return self.n.call("Grant.Issue", {"principal_id": self.actor["principal_id"], "actions": actions,
                                          "ttl_seconds": 300, "units": 8, "model_calls": 8, **scope})

    def opened(self, *, queued=False, max_turns=3, grant=None, **options):
        return self.n.call("Session.Open", {"key": key(), "goal": "Observe the requested local resource.",
                                            "skill_ref": self.skill, "model_ref": self.model,
                                            "delegation_id": (grant or self.grant)["delegation_ref"]["resource_id"],
                                            "context_policy": "ReferencesOnly", "max_turns": max_turns,
                                            "provider_queue": queued, "tool_context_window": 1, **options}, self.actor_token)

    def pause(self, view, reason):
        return self.n.call("Session.Pause", {**target(view["reference"]), "reason": reason}, self.actor_token)

    def current(self, view):
        return self.n.call("Session.Get", {"id": view["reference"]["resource_id"]}, self.actor_token)

    def claim(self, view, claim_key):
        return self.n.call("Model.Claim", {"session_id": view["reference"]["resource_id"], "key": claim_key},
                           self.provider["token"])

    def claim_next(self):
        return self.n.call("Model.WaitNext", {"model_ref": self.model, "key": key(), "timeout_ms": 75},
                           self.provider["token"])

    def decision(self, view, lease, decision):
        return self.n.call("Model.Respond", {"session_id": view["reference"]["resource_id"],
                                            "lease_id": lease["lease_id"], "decision": decision}, self.provider["token"])

    def refuse(self, code, method, arguments, token=None):
        with self.assertRaises(Failure) as denied:
            self.n.call(method, arguments, self.actor_token if token is None else token)
        self.assertEqual(denied.exception.code, code, denied.exception)

    def budget(self, grant=None):
        return self.n.call("Budget.Get", {"id": (grant or self.grant)["budget_ref"]["resource_id"]}, self.actor_token)

    def restart(self):
        self.n.stop(signal.SIGKILL)
        self.n.start()

    def test_ready_pause_is_conditional_durable_and_skips_only_this_queued_session(self):
        first = self.opened(queued=True)
        before = self.budget()
        sid = first["reference"]["resource_id"]
        pause_target = target(first["reference"])
        outsider, _ = self.n.agent(["task.read"])
        self.refuse("PermissionDenied", "Session.Pause", {**pause_target, "reason": "unrelated actor"}, outsider["token"])
        self.refuse("InvalidArgument", "Session.Pause", {**pause_target, "reason": "\n"})
        barrier = Barrier(2)

        def contender(reason):
            barrier.wait(5)
            try:
                return self.pause(first, reason)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(2) as workers:
            results = list(workers.map(contender, ("Check prior observations", "Hold for maintenance")))
        self.assertEqual([result for result in results if isinstance(result, str)], ["Conflict"])
        paused, = [result for result in results if isinstance(result, dict)]
        self.assertEqual((paused["session"]["lifecycle"], paused["session"]["pause_reason"]), ("Paused", "OperatorPaused"))
        hold = paused["session"]["operator_pause"]
        self.assertEqual(hold["requested_by"], self.actor["principal_id"])
        self.assertIsNone(hold["after_task_id"])
        self.assertEqual(self.pause(paused, hold["reason"]), paused)
        self.assertEqual(self.claim_next(), {"available": False})
        self.refuse("Conflict", "Model.Claim", {"session_id": sid, "key": key()}, self.provider["token"])
        self.assertEqual(self.budget(), before)

        self.restart()
        self.assertEqual(self.current(first), paused)
        second = self.opened(queued=True)
        available = self.claim_next()
        self.assertEqual(available["session_id"], second["reference"]["resource_id"])
        self.decision(second, available, {"action": "finish", "summary": "Independent session finished."})
        self.assertEqual(self.claim_next(), {"available": False})
        self.assertEqual(self.budget()["model_calls_used"], before["model_calls_used"] + 1)
        self.refuse("Conflict", "Session.Resume", pause_target)
        resumed = self.n.cli(self.actor_file, "session-resume", sid, "--revision", paused["reference"]["version"])
        self.assertEqual(resumed["session"]["lifecycle"], "AwaitingModel")
        self.assertNotIn("operator_pause", resumed["session"])
        claim = self.claim_next()
        self.assertEqual(claim["session_id"], sid)
        self.assertEqual(claim["turn"], 1)
        self.decision(first, claim, {"action": "finish", "summary": "Resumed successfully."})

    def test_pending_approval_settles_exact_task_before_pause_without_new_model_call(self):
        volume = self.n.perform("volume.create", {"name": "agent-pause-volume"})
        path = self.n.state / "volumes" / volume["resource_id"] / "data" / "approved"
        session = self.opened(max_turns=1)
        lease = self.claim(session, "original")
        in_flight = self.current(session)
        self.refuse("PreconditionFailed", "Session.Pause", {**target(in_flight["reference"]), "reason": "Pause"})
        self.assertEqual(self.current(session), in_flight)
        self.assertEqual(self.claim(session, "original"), {**lease, "recovered": True})
        accepted = self.decision(session, lease, {"action": "tool", "tool": "volume.write",
                                                  "input": {**target(volume), "name": path.name, "content": "approved effect"}})
        task_id = accepted["task_id"]
        task = self.n.wait(task_id)
        self.assertEqual(task["lifecycle"], "WaitingApproval", task)
        self.assertFalse(path.exists())
        queued = self.current(session)
        route = queued["reference"]
        waiting = self.n.cli(self.actor_file, "session-pause", route["resource_id"], "--revision", route["version"],
                             "--reason", "Stop after the approved write")
        self.assertEqual(waiting["session"]["lifecycle"], "WaitingTask")
        self.assertEqual(waiting["session"]["operator_pause"]["after_task_id"], task_id)
        self.assertEqual(self.pause(waiting, "Stop after the approved write"), waiting)
        self.refuse("Conflict", "Session.Pause", {**target(route), "reason": "Stale"})
        self.refuse("PreconditionFailed", "Session.Resume", target(waiting["reference"]))
        self.restart()
        self.assertEqual(self.current(session), waiting)
        self.n.approve(task_id)
        completed = self.n.wait(task_id)
        self.assertEqual((completed["lifecycle"], completed["outcome_knowledge"]), ("Succeeded", "Known"))
        self.assertEqual(path.read_text(), "approved effect")
        parked = self.current(session)
        self.assertEqual((parked["session"]["lifecycle"], parked["session"]["pause_reason"]), ("Paused", "OperatorPaused"))
        self.assertEqual(parked["session"]["task_ref"], self.n.get(task_id)["reference"])
        self.assertEqual(self.claim(session, "original")["completed"], accepted)
        self.assertEqual((self.budget()["model_calls_used"], self.budget()["spent"]), (1, 1))
        self.refuse("QuotaExceeded", "Session.Resume", target(parked["reference"]))
        extended = self.n.call("Session.Extend", {**target(parked["reference"]), "max_turns": 2}, self.actor_token)
        self.assertEqual(extended["session"]["lifecycle"], "Paused")
        self.n.call("Session.Resume", target(extended["reference"]), self.actor_token)
        next_lease = self.claim(session, "after-pause")
        self.assertEqual(next_lease["context"]["last_tool"]["task_id"], task_id)
        self.decision(session, next_lease, {"action": "finish", "summary": "Approved write confirmed."})
        history = self.n.call("Session.History", {"id": route["resource_id"]}, self.actor_token)
        self.assertEqual([entry["task_id"] for entry in history["tasks"]], [task_id])
        self.assertEqual((self.budget()["model_calls_used"], self.budget()["spent"]), (2, 1))
        self.n.stop()
        subprocess.run(self.n.daemon + ["verify"], check=True, capture_output=True, timeout=30)

    def test_pause_preserves_attached_input_until_terminal_cancel(self):
        node_id = self.n.call("Node.Get", {})["reference"]["resource_id"]
        self.issue(["artifact.upload", "artifact.withdraw"], [node_id])
        payload = json.dumps({"observation": "approved data"}).encode()
        stage = self.n.call("Artifact.Stage", upload_request(payload, declared_media_type="application/json"), self.actor_token)
        self.n.call("Artifact.Stage.PutChunk", chunk(stage, 0, payload), self.actor_token)
        reference = self.n.call("Artifact.Publish", {"stage_id": stage["stage_id"]}, self.actor_token)["artifact_ref"]
        grant = self.issue(self.actions + ["session.context.read", "artifact.read"],
                           [node_id, self.model["resource_id"], reference["resource_id"], "profile-local"])
        session = self.opened(grant=grant, input_artifacts=[{"name": "pinned", "reference": reference, "kind": "Json"}])
        parked = self.pause(session, "Keep evidence during the pause")
        withdrawal = {"reference": reference, "reason": "session closed"}
        self.refuse("Conflict", "Artifact.Withdraw", withdrawal)
        self.n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.refuse("PermissionDenied", "Session.Resume", target(parked["reference"]))
        self.refuse("Conflict", "Artifact.Withdraw", withdrawal)
        cancelled = self.n.call("Session.Cancel", target(parked["reference"]), self.actor_token)
        self.assertEqual(cancelled["session"]["lifecycle"], "Cancelled")
        self.assertNotIn("operator_pause", cancelled["session"])
        self.n.call("Artifact.Withdraw", withdrawal, self.actor_token)

    def test_unknown_strict_turn_cannot_be_marked_operator_paused(self):
        session = self.opened()
        lease = self.claim(session, "strict-unknown")
        sid = session["reference"]["resource_id"]
        result = self.n.call("Model.ReportFailure", {"session_id": sid, "lease_id": lease["lease_id"],
                                                    "model_ref": self.model, "outcome": "Unknown"}, self.provider["token"])
        current = target(result["session_ref"])
        self.refuse("PreconditionFailed", "Session.Pause", {**current, "reason": "Do not relabel uncertainty"})
        self.refuse("PreconditionFailed", "Session.Resume", current)
        self.refuse("OutcomeUnknown", "Model.Claim", {"session_id": sid, "key": key()}, self.provider["token"])
        unchanged = self.current(session)
        self.assertEqual(unchanged["reference"], result["session_ref"])
        self.assertEqual(unchanged["session"]["outcome_knowledge"], "Unknown")
        self.assertNotIn("operator_pause", unchanged["session"])
        self.assertEqual(self.budget()["model_calls_used"], 1)
