"""Verify a replacement provider grant cannot reopen an older inference lease."""
from datetime import datetime, timezone
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key, target
from test_model_fallback import publish_skill


STATUS_TOOL = {"action": "tool", "tool": "system.status", "input": {}}


class ModelProviderAuthorityTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.provider = self.node.call("Agent.Register", {"name": "model-lease-provider"})
        self.owner = self.node.call("Agent.Register", {"name": "model-lease-session-owner"})
        self.model = self.register_model("same-provider-target")
        self.other_model = self.register_model("same-provider-independent-target")
        self.grant_a = self.serve_grant(self.model)
        self.serve_grant(self.other_model)
        self.skill = publish_skill(self.node)
        self.session_grant = self.node.call("Grant.Issue", {
            "principal_id": self.owner["principal_id"],
            "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "tool_targets": [self.model["resource_id"], self.other_model["resource_id"], "profile-local"],
            "model_calls": 12,
            "model_lease_ms": 540_000,
            "units": 12,
            "ttl_seconds": 240,
        })

    def register_model(self, name):
        return self.node.call("Model.Register", {
            "name": name,
            "revision": "1",
            "provider_principal_id": self.provider["principal_id"],
            "inference_outcome_policy": "Strict",
            "lease_seconds": 45,
            "max_in_flight": 3,
            "max_context_bytes": 32_768,
            "max_output_bytes": 8192,
        })

    def serve_grant(self, model):
        return self.node.call("Grant.Issue", {
            "principal_id": self.provider["principal_id"],
            "actions": ["model.serve"],
            "tool_targets": [model["resource_id"]],
            "units": 1,
            "ttl_seconds": 240,
        })

    def open_session(self, model, queued):
        return self.node.call("Session.Open", {
            "key": key(),
            "provider_queue": queued,
            "goal": "Read the current local system status.",
            "skill_ref": self.skill,
            "model_ref": model,
            "context_policy": "LocalToolResults",
            "tool_context_window": 2,
            "max_turns": 4,
            "delegation_id": self.session_grant["delegation_ref"]["resource_id"],
        }, self.owner["token"])["reference"]["resource_id"]

    def provider_call(self, method, params):
        return self.node.call(method, params, self.provider["token"])

    def history(self, session):
        return self.node.call("Session.History", {"id": session}, self.owner["token"])

    def charge(self):
        budget = self.node.call("Budget.Get", {"id": self.session_grant["budget_ref"]["resource_id"]})
        return budget["model_calls_used"], budget["model_lease_ms_charged"], budget["spent"]

    def task_ids(self):
        snapshot = self.node.call("Resource.List", {"kind": "Task"})
        return {ref["resource_id"] for ref in snapshot["items"]}

    def reject_provider(self, code, method, params):
        with self.assertRaises(Failure) as rejected:
            self.provider_call(method, params)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    def pending_report(self, model, session, claim):
        return {"session_id": session, "model_ref": model, "lease_id": claim["lease_id"], "outcome": "NotStarted"}

    def response(self, session, claim, decision=STATUS_TOOL):
        return {"session_id": session, "lease_id": claim["lease_id"], "decision": decision}

    def result(self, task_id):
        task = self.node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        reference, = task["result"]["artifact_refs"]
        artifact = self.node.call("Artifact.Read", {"reference": reference})
        self.assertTrue(artifact["authority_id"], artifact)
        return artifact

    def verify(self):
        self.node.stop()
        subprocess.run([*self.node.daemon, "verify"], check=True, capture_output=True, timeout=20)

    def test_replacement_grant_fences_old_direct_and_queued_leases_after_restart(self):
        n = self.node
        manual = self.open_session(self.model, queued=False)
        queued = self.open_session(self.model, queued=True)
        independent = self.open_session(self.other_model, queued=False)

        already_done = {"model_ref": self.model, "key": key()}
        first = self.provider_call("Model.ClaimNext", already_done)
        self.assertEqual(first["session_id"], queued)
        prior_response = self.response(queued, first)
        settled = self.provider_call("Model.Respond", prior_response)
        prior_task = settled["task_id"]
        prior_result = self.result(prior_task)

        manual_request = {"session_id": manual, "key": key()}
        manual_claim = self.provider_call("Model.Claim", manual_request)
        queue_request = {"model_ref": self.model, "key": key()}
        queue_claim = self.provider_call("Model.ClaimNext", queue_request)
        self.assertEqual((queue_claim["session_id"], queue_claim["turn"]), (queued, 2))
        self.assertEqual(queue_claim["context"]["last_tool"]["local_tool_result"], prior_result)
        old_status = {**queue_request, "lease_id": queue_claim["lease_id"]}
        self.assertEqual(self.provider_call("Model.ClaimStatus", old_status), {"status": "Pending"})

        other_claim = self.provider_call("Model.Claim", {"session_id": independent, "key": key()})
        revocation = n.call("Grant.Revoke", target(self.grant_a["grant_ref"]))
        self.assertEqual(revocation["status"], "Partial", revocation)
        self.reject_provider("PermissionDenied", "Model.ClaimStatus", old_status)
        self.reject_provider("PermissionDenied", "Model.Claim", manual_request)
        self.reject_provider("PermissionDenied", "Model.Respond", self.response(manual, manual_claim))
        self.reject_provider("PermissionDenied", "Model.Respond", prior_response)

        # Removing the authority for one exact Model leaves the other Model's live lease intact.
        other_result = self.provider_call("Model.Respond", self.response(independent, other_claim, {
            "action": "finish", "summary": "Independent model completed with its original scope.",
        }))
        self.assertEqual(other_result["lifecycle"], "Succeeded", other_result)
        charge = self.charge()
        self.assertEqual((charge[0], charge[2]), (4, 1), charge)
        self.assertEqual(self.task_ids(), {prior_task})

        grant_b = self.serve_grant(self.model)
        self.assertNotEqual(grant_b["delegation_ref"], self.grant_a["delegation_ref"])
        n.stop(signal.SIGKILL)
        n.start()
        for claim in (manual_claim, queue_claim):
            self.assertGreater(datetime.fromisoformat(claim["expires_at"]), datetime.now(timezone.utc),
                               "the lease must still be within its original deadline")

        self.reject_provider("Fenced", "Model.Claim", manual_request)
        self.reject_provider("Fenced", "Model.ClaimNext", queue_request)
        self.assertEqual(self.provider_call("Model.ClaimStatus", old_status), {"status": "Fenced"})
        for session, claim in ((manual, manual_claim), (queued, queue_claim)):
            self.reject_provider("Fenced", "Model.Respond", self.response(session, claim))
            report = self.pending_report(self.model, session, claim)
            self.reject_provider("Fenced", "Model.ReportFailure", report)
            self.reject_provider("Fenced", "Model.ReportFailure", dict(report, outcome="Unknown"))
            last = self.history(session)["turns"][-1]
            self.assertEqual(last["lease_id"], claim["lease_id"])
            self.assertIsNone(last["decision"], last)
            self.assertIsNone(last["task_id"], last)
        self.assertEqual(self.charge(), charge)
        self.assertEqual(self.task_ids(), {prior_task})

        # The already committed decision and referenced tool result survive revoked authority.
        done_status = {**already_done, "lease_id": first["lease_id"]}
        self.assertEqual(self.provider_call("Model.ClaimStatus", done_status), {"status": "Settled"})
        self.assertEqual(self.provider_call("Model.Respond", prior_response), settled)
        self.assertEqual(self.provider_call("Model.ClaimNext", already_done)["completed"], settled)
        self.assertEqual(self.history(queued)["turns"][0]["decision"], settled)
        self.assertEqual(self.result(prior_task), prior_result)

        replacement = self.open_session(self.model, queued=True)
        fresh_request = {"model_ref": self.model, "key": key()}
        fresh = self.provider_call("Model.ClaimNext", fresh_request)
        self.assertEqual((fresh["session_id"], fresh["turn"], fresh["recovered"]), (replacement, 1, False))
        self.assertNotIn(fresh["lease_id"], {manual_claim["lease_id"], queue_claim["lease_id"]})
        self.open_session(self.model, queued=True)
        self.assertEqual(self.provider_call("Model.ClaimNext", {"model_ref": self.model, "key": key()}),
                         {"available": False}, "both fenced calls still occupy their provider slots")
        decision = self.response(replacement, fresh)
        receipt = self.provider_call("Model.Respond", decision)
        task = receipt["task_id"]
        self.assertNotEqual(task, prior_task)
        self.result(task)
        self.assertEqual(self.provider_call("Model.Respond", decision), receipt)
        self.assertEqual(self.provider_call("Model.ClaimNext", fresh_request)["completed"], receipt)
        self.assertEqual(self.history(replacement)["tasks"], [{"task_id": task, "turn": 1, "attempt": 1}])
        self.assertEqual(self.task_ids(), {prior_task, task})
        calls, _, spent = self.charge()
        self.assertEqual((calls, spent), (5, 2))
        self.verify()

    def test_revoked_provider_delegation_stays_fenced_with_its_grant_still_active(self):
        n = self.node
        session = self.open_session(self.model, queued=False)
        request = {"session_id": session, "key": key()}
        claim = self.provider_call("Model.Claim", request)
        n.call("Delegation.Revoke", target(self.grant_a["delegation_ref"]))
        grant = n.get(self.grant_a["grant_ref"]["resource_id"])
        self.assertEqual(grant["value"]["lifecycle"], "Active", grant)
        self.serve_grant(self.model)

        self.reject_provider("Fenced", "Model.Claim", request)
        self.reject_provider("Fenced", "Model.Respond", self.response(session, claim))
        self.reject_provider("Fenced", "Model.ReportFailure", self.pending_report(self.model, session, claim))
        self.assertEqual(self.history(session)["tasks"], [])
        self.assertEqual(self.task_ids(), set())
        self.assertEqual(self.charge(), (1, 45_000, 0))


if __name__ == "__main__":
    unittest.main()
