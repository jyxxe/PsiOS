"""Record a genuinely returned inference as late evidence without dispatching it."""
from contextlib import closing
from datetime import datetime, timezone
import hashlib
import json
import os
import signal
import sqlite3
import subprocess
from time import monotonic, sleep
import unittest

from model_delivery import interrupt_worker_delivery
from support import Failure, Node, key, target
from test_model_provider_outcome import serve_inference


OBSERVE = "Model.ObserveLateDecision"
STATUS = {"action": "tool", "tool": "system.status", "input": {}}
WIRE_USAGE = {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
USAGE = {"input_tokens": 7, "output_tokens": 3}


def canonical(value):
    return json.dumps(value, separators=(",", ":"), sort_keys=True).encode()


def digest(value):
    return "sha256:" + hashlib.sha256(canonical(value)).hexdigest()


class ModelLateDecisionTests(unittest.TestCase):
    lease_seconds = 5

    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        n = self.node
        self.provider = n.call("Agent.Register", {"name": "late-response-provider"})
        self.other = n.call("Agent.Register", {"name": "unrelated-late-provider"})
        self.owner = n.call("Agent.Register", {"name": "late-response-session-owner"})
        self.model = n.call("Model.Register", {
            "name": "strict-fixture", "revision": "1", "provider_principal_id": self.provider["principal_id"],
            "inference_outcome_policy": "Strict", "lease_seconds": self.lease_seconds, "max_in_flight": 2,
            "max_context_bytes": 16 * 1024, "max_output_bytes": 2048,
        })
        self.provider_grant = self.serve_grant(self.provider)
        self.serve_grant(self.other)
        self.skill = n.call("Skill.Publish", {
            "name": "late-response-status", "version": "1", "license": "0BSD", "source": "local-test",
            "tools": ["system.status"], "instructions": "Use the local system status tool.",
        })
        self.session_grant = n.call("Grant.Issue", {
            "principal_id": self.owner["principal_id"], "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "tool_targets": [self.model["resource_id"], self.skill["resource_id"], "profile-local"],
            "model_calls": 4, "model_lease_ms": 4 * self.lease_seconds * 1000, "units": 4, "ttl_seconds": 180,
        })
        self.provider_token = n.base / "late-provider.token"
        self.provider_token.write_text(self.provider["token"])
        self.provider_token.chmod(0o600)
        self.journal = n.base / "late-journal"
        self.journal.mkdir(mode=0o700)
        self.pending = self.journal / "psios-model-pending.json"
        self.endpoint = n.base / "late-inference.sock"
        self.config = n.base / "late-model.json"
        self.config.write_text(json.dumps({
            "socket": str(self.endpoint), "server_uid": os.geteuid(), "model_ref": self.model,
            "model_alias": "strict-fixture", "max_tokens": 112, "timeout_ms": 1500,
        }))

    def serve_grant(self, provider):
        return self.node.call("Grant.Issue", {
            "principal_id": provider["principal_id"], "actions": ["model.serve"],
            "tool_targets": [self.model["resource_id"]], "units": 4, "ttl_seconds": 180,
        })

    def open_session(self, queued=False):
        return self.node.call("Session.Open", {
            "key": key(), "goal": "Read the current system status.", "model_ref": self.model,
            "skill_ref": self.skill, "context_policy": "ReferencesOnly", "max_turns": 3,
            "provider_queue": queued,
            "delegation_id": self.session_grant["delegation_ref"]["resource_id"],
        }, self.owner["token"])["reference"]["resource_id"]

    def provider_call(self, method, params):
        return self.node.call(method, params, self.provider["token"])

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as caught:
            self.node.call(method, params, self.provider["token"] if token is None else token)
        self.assertEqual(caught.exception.code, code, caught.exception)
        return caught.exception.args[0]

    def session(self, session):
        return self.node.call("Session.Get", {"id": session}, self.owner["token"])

    def history(self, session):
        return self.node.call("Session.History", {"id": session}, self.owner["token"])

    def budget(self):
        return self.node.call("Budget.Get", {"id": self.session_grant["budget_ref"]["resource_id"]}, self.owner["token"])

    def failure_report(self, session, lease, outcome):
        return {"session_id": session, "lease_id": lease, "model_ref": self.model, "outcome": outcome}

    def verify(self):
        self.node.stop()
        result = subprocess.check_output([*self.node.daemon, "verify"], timeout=20)
        report = json.loads(result)
        self.assertIs(report["migration_required"], False, report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"], report["witness"]),
                         ("Verified", "Verified", "Confirmed"), report)

    def test_http_tool_decision_survives_two_lost_transmissions_as_digest_evidence(self):
        n = self.node
        session = self.open_session()
        unrelated_session = self.open_session()
        claim_request = {"session_id": session, "key": "late-http-attempt"}
        args = ["model-step", session, "--key", claim_request["key"],
                "--decision-dir", self.journal, "--local-model", self.config]
        os_boot = n.call("Node.Get", {})["state"]["boot_id"]
        with serve_inference(self.endpoint, decision=STATUS, usage=WIRE_USAGE) as requests:
            with interrupt_worker_delivery(n, self.provider_token, args, committed=False):
                raw = self.pending.read_bytes()
                saved = json.loads(raw)["record"]
                self.assertEqual(saved["format"], 4, saved)
                self.assertEqual(saved["late_decision_model_ref"], self.model, saved)
                self.assertEqual(saved["claim_key"], claim_request["key"], saved)
                decision_request = saved["request"]
                lease = decision_request["lease_id"]
                self.assertEqual(decision_request, {
                    "session_id": session, "lease_id": lease, "decision": STATUS, "usage": USAGE,
                })
                self.assertEqual(len(requests), 1)
                self.assertEqual(requests[0]["model"], "strict-fixture", requests)
                issued = self.session(session)
                self.assertEqual(issued["session"]["lifecycle"], "ModelInFlight", issued)
                usage_ref = issued["session"]["current_usage_ref"]
                claim = self.provider_call("Model.Claim", claim_request)
                self.assertIs(claim["late_decision_observation"], True, claim)
                self.assertEqual(claim["model_ref"], self.model, claim)
                self.assertEqual(claim["lease_id"], lease, claim)
                self.assertIsNone(self.history(session)["turns"][0]["decision"])
                pending = self.rejected("PreconditionFailed", OBSERVE, decision_request)
                self.assertEqual(pending["retry_condition"], "SameRequestAfterBackoff", pending)

            # Prevent the first normal response from reaching Core and let the
            # original Linux lease expire while the authority is stopped.
            n.stop(signal.SIGKILL)
            expiry = datetime.fromisoformat(claim["expires_at"])
            sleep(max(0, (expiry - datetime.now(timezone.utc)).total_seconds()) + 0.1)
            n.start()
            self.assertEqual(n.call("Node.Get", {})["state"]["boot_id"], os_boot)
            deadline = monotonic() + 8
            while True:
                paused = self.session(session)
                if paused["session"]["lifecycle"] == "Paused":
                    break
                self.assertLess(monotonic(), deadline, paused)
                sleep(0.05)
            self.assertEqual(paused["session"]["outcome_knowledge"], "Unknown", paused)
            self.assertEqual(paused["session"]["model_inference_failure"]["source"], "LeaseExpired", paused)
            initial_history = self.history(session)
            turn, = initial_history["turns"]
            self.assertIsNone(turn["decision"], turn)
            self.assertIsNone(turn.get("late_decision_observation"), turn)
            self.assertEqual(initial_history["tasks"], [])
            budget = self.budget()
            self.assertEqual((budget["model_calls_used"], budget["model_lease_ms_charged"], budget["spent"]),
                             (1, self.lease_seconds * 1000, 0), budget)
            usage = n.call("Resource.Resolve", {"reference": usage_ref})["value"]
            self.assertNotIn("reported_tokens", usage)
            foreign = self.rejected("PermissionDenied", OBSERVE, decision_request, self.other["token"])
            self.assertNotEqual(foreign.get("details", {}).get("reason"), "OriginalProviderAuthorityUnavailable")
            self.rejected("PermissionDenied", OBSERVE, {**decision_request, "session_id": unrelated_session})
            self.rejected("Fenced", "Model.Respond", decision_request)
            self.rejected("Fenced", "Model.ReportFailure", self.failure_report(session, lease, "NotStarted"))

            # Commit only the late observation, then lose the reply. The SDK
            # must retain the original delivery and recover one exact receipt.
            with interrupt_worker_delivery(n, self.provider_token, args, committed=True, method=OBSERVE):
                self.assertEqual(self.pending.read_bytes(), raw)
                observation = self.history(session)["turns"][0]["late_decision_observation"]
                self.assertEqual(len(requests), 1)
                self.assertEqual(self.session(session), paused)
            n.stop(signal.SIGKILL)
            n.start()
            observed = n.cli(self.provider_token, *args)
            self.assertEqual(observed, {"status": "Recorded", "observation": observation})
            self.assertEqual(len(requests), 1, "late evidence must never send another inference POST")
            self.assertEqual(list(self.journal.iterdir()), [])

        self.assertEqual(observation["lease_id"], lease)
        self.assertEqual(observation["model_ref"], self.model)
        self.assertEqual(observation["issued_session_ref"], issued["reference"])
        self.assertEqual(observation["unknown_session_ref"], paused["reference"])
        self.assertEqual(observation["provider_principal_id"], self.provider["principal_id"])
        self.assertEqual(observation["provider_authority_digest"], usage["provider_authority_digest"])
        self.assertEqual(observation["decision_digest"], digest(STATUS))
        self.assertEqual(observation["usage_digest"], digest(USAGE))
        self.assertEqual(observation["output_bytes"], len(canonical(STATUS)))
        self.assertIsNotNone(datetime.fromisoformat(observation["observed_at"]).utcoffset())
        self.assertEqual(set(observation), {
            "lease_id", "model_ref", "issued_session_ref", "unknown_session_ref", "provider_principal_id",
            "provider_authority_digest", "decision_digest", "output_bytes", "observed_at", "usage_digest",
        })
        self.assertEqual(self.provider_call(OBSERVE, decision_request), observed)
        self.rejected("IdempotencyConflict", OBSERVE, {
            **decision_request, "decision": {"action": "finish", "summary": "different reply"},
        })
        self.rejected("IdempotencyConflict", OBSERVE, {**decision_request, "usage": {**USAGE, "output_tokens": 4}})
        self.rejected("PreconditionFailed", "Session.Resume", target(paused["reference"]), self.owner["token"])
        self.assertEqual(self.session(session), paused)
        self.assertEqual(self.provider_call("Model.Claim", claim_request)["completed"], turn["inference_failure"])
        history = self.history(session)
        self.assertEqual(history["turns"], [{**turn, "late_decision_observation": observation}])
        self.assertEqual(history["tasks"], [])
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])
        self.assertEqual(self.budget(), budget)
        self.assertEqual(n.get(usage_ref["resource_id"])["reference"], usage_ref)

        n.call("Grant.Revoke", target(self.provider_grant["grant_ref"]))
        self.rejected("PermissionDenied", OBSERVE, decision_request)
        replacement = self.serve_grant(self.provider)
        self.assertNotEqual(replacement["grant_ref"], self.provider_grant["grant_ref"])
        blocked = self.rejected("PermissionDenied", OBSERVE, decision_request)
        self.assertEqual(blocked.get("details"), {"reason": "OriginalProviderAuthorityUnavailable"})
        self.assertEqual(self.history(session), history)
        self.verify()

        # Check the actual offline catalogue as well as the public projection.
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            self.assertEqual(db.execute("""
                SELECT count(*) FROM versions WHERE instr(body,?)>0 OR instr(spec,?)>0
            """, (canonical(STATUS).decode(), canonical(STATUS).decode())).fetchone(), (0,))
            self.assertEqual(db.execute("SELECT count(*) FROM session_turns WHERE task IS NOT NULL").fetchone(), (0,))
            stored, = db.execute("SELECT observation,observation_digest FROM model_late_decisions WHERE lease=?",
                                 (lease,)).fetchall()
            self.assertEqual(json.loads(stored[0]), observation)
            domain = "psios-model-late-decision-v1"
            self.assertEqual(stored[1], digest([domain, observation]))
            altered = {**observation, "unknown_session_ref": issued["reference"]}
            db.execute("UPDATE model_late_decisions SET observation=?,observation_digest=? WHERE lease=?",
                       (canonical(altered).decode(), digest([domain, altered]), lease))
        bad = subprocess.run([*n.daemon, "verify"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(bad.returncode, 0, bad.stdout)
        self.assertIn("RecoveryRequired", bad.stderr)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("UPDATE model_late_decisions SET observation=?,observation_digest=? WHERE lease=?",
                       (*stored, lease))
        self.verify()

    def test_replacement_cannot_create_observation_for_unrecorded_original_unknown_lease(self):
        n = self.node
        session = self.open_session()
        args = ["model-step", session, "--key", "revoked-late-http", "--decision-dir", self.journal,
                "--local-model", self.config]
        with serve_inference(self.endpoint, decision=STATUS) as requests:
            with interrupt_worker_delivery(n, self.provider_token, args, committed=False):
                decision = json.loads(self.pending.read_bytes())["record"]["request"]
                lease = decision["lease_id"]
                before = self.provider_call("Model.ReportFailure", self.failure_report(session, lease, "Unknown"))
                self.assertEqual((before["lifecycle"], before["outcome_knowledge"]), ("Paused", "Unknown"))
            n.call("Grant.Revoke", target(self.provider_grant["grant_ref"]))
            self.serve_grant(self.provider)
            rejected = self.rejected("PermissionDenied", OBSERVE, decision)
            self.assertEqual(rejected.get("details"), {"reason": "OriginalProviderAuthorityUnavailable"})
            command = [n.binaries / "psiosctl", "--socket", n.socket, "--token-file", self.provider_token, *args]
            replay = subprocess.run(command, capture_output=True, text=True, timeout=12)
            self.assertNotEqual(replay.returncode, 0, replay.stdout)
            self.assertIn("PermissionDenied", replay.stderr)
            self.assertEqual(len(requests), 1)
            self.assertEqual(list(self.journal.iterdir()), [])
        history = self.history(session)
        turn, = history["turns"]
        self.assertIsNone(turn["decision"], turn)
        self.assertIsNone(turn.get("late_decision_observation"), turn)
        self.assertEqual(history["tasks"], [])
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])
        self.assertEqual(self.session(session)["reference"], before["session_ref"])
        self.assertEqual(self.budget()["model_lease_ms_charged"], self.lease_seconds * 1000)
        self.verify()

    def test_queued_failure_status_does_not_discard_the_original_http_decision(self):
        n = self.node
        session = self.open_session(queued=True)
        model_file = n.base / "queued-model.json"
        model_file.write_text(json.dumps(self.model))
        args = ["model-worker", model_file, "--decision-dir", self.journal,
                "--local-model", self.config, "--wait-ms", "1000", "--once"]
        with serve_inference(self.endpoint, decision=STATUS, usage=WIRE_USAGE) as requests:
            with interrupt_worker_delivery(n, self.provider_token, args, committed=False):
                saved = json.loads(self.pending.read_bytes())["record"]
                self.assertEqual(saved["format"], 4, saved)
                self.assertEqual(saved["queue_model"], self.model, saved)
                self.assertEqual(saved["request"]["session_id"], session)
                lease = saved["request"]["lease_id"]
                failed = self.provider_call("Model.ReportFailure", self.failure_report(session, lease, "Unknown"))
            self.assertEqual(self.provider_call("Model.ClaimStatus", {
                "model_ref": self.model, "key": saved["claim_key"], "lease_id": lease,
            }), {"status": "Settled"})
            n.stop(signal.SIGKILL)
            n.start()
            result = n.cli(self.provider_token, *args)
            self.assertEqual(result["status"], "Recorded", result)
            evidence = result["observation"]
            self.assertEqual(evidence["unknown_session_ref"], failed["session_ref"])
            self.assertEqual(evidence["decision_digest"], digest(STATUS))
            self.assertEqual(evidence["usage_digest"], digest(USAGE))
            self.assertEqual(len(requests), 1)
            self.assertEqual(list(self.journal.iterdir()), [])
        history = self.history(session)
        self.assertEqual(history["turns"][0]["late_decision_observation"], evidence)
        self.assertEqual(history["tasks"], [])
        self.assertEqual(self.budget()["spent"], 0)
        self.verify()


if __name__ == "__main__":
    unittest.main()
