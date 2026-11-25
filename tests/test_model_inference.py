"""Strict inference outcome accounting through a real local authority socket."""
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from test_model_fallback import publish_skill, register_provider


class StrictModelInference(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.provider, _ = self.n.agent(["model.serve"])
        self.model = self.register()
        self.other, self.alternate = register_provider(self.n, "other-provider")
        self.skill = publish_skill(self.n)
        self.owner = self.n.call("Agent.Register", {"name": "strict-inference-owner"})
        self.authorization = self.grant()

    def register(self, **overrides):
        return self.n.call("Model.Register", {"name": "strict-fixture", "revision": key(),
            "provider_principal_id": self.provider["principal_id"], "max_context_bytes": 16384,
            "max_output_bytes": 8192, "lease_seconds": 8, "max_in_flight": 1,
            "inference_outcome_policy": "Strict", **overrides})

    def grant(self, calls=8):
        return self.n.call("Grant.Issue", {"principal_id": self.owner["principal_id"],
            "actions": ["task.submit", "task.read", "session.run", "session.failover", "system.status"],
            "tool_targets": [self.model["resource_id"], self.alternate["resource_id"], "profile-local"],
            "units": 8, "model_calls": calls, "model_lease_ms": 48000, "ttl_seconds": 180})

    def open(self, queued=True, grant=None):
        return self.n.call("Session.Open", {"key": key(), "provider_queue": queued,
            "goal": "Inspect local status", "skill_ref": self.skill, "model_ref": self.model,
            "fallback_model_refs": [self.alternate], "context_policy": "ReferencesOnly", "max_turns": 4,
            "delegation_id": (grant or self.authorization)["delegation_ref"]["resource_id"]}, self.owner["token"])["reference"]["resource_id"]

    def get(self, sid):
        return self.n.call("Session.Get", {"id": sid}, self.owner["token"])

    def claim(self, sid=None, claim_key=None):
        if sid is not None:
            return self.n.call("Model.Claim", {"session_id": sid, "key": claim_key or key()}, self.provider["token"])
        return self.n.call("Model.ClaimNext", {"model_ref": self.model, "key": claim_key or key()}, self.provider["token"])

    def budget(self, grant=None):
        return self.n.call("Budget.Get", {"id": (grant or self.authorization)["budget_ref"]["resource_id"]})

    def report(self, sid, claim, outcome):
        return {"session_id": sid, "model_ref": self.model, "lease_id": claim["lease_id"], "outcome": outcome}

    def deny(self, code, method, params, actor):
        with self.assertRaises(Failure) as caught:
            self.n.call(method, params, actor["token"])
        self.assertEqual(caught.exception.code, code, caught.exception)

    def verify(self):
        self.n.stop()
        subprocess.run(self.n.daemon + ["verify"], check=True, capture_output=True, timeout=15)

    def test_not_started_requires_explicit_resume_and_new_budget(self):
        sid = self.open()
        claim_key = key()
        first = self.claim(claim_key=claim_key)
        self.assertEqual(first["inference_outcome_policy"], "Strict")
        request = self.report(sid, first, "NotStarted")
        self.deny("PermissionDenied", "Model.ReportFailure", request, self.other)
        self.deny("PreconditionFailed", "Model.ReportFailure", dict(request, model_ref=self.alternate), self.provider)
        self.deny("InvalidArgument", "Model.ReportFailure", dict(request, outcome="Known"), self.provider)
        used = self.budget()
        accepted = self.n.call("Model.ReportFailure", request, self.provider["token"])
        self.assertEqual((accepted["lifecycle"], accepted["outcome_knowledge"]), ("Paused", "NotStarted"))
        self.assertEqual(accepted["session_ref"], self.get(sid)["reference"])
        charged = self.budget()
        self.assertEqual((used["model_calls_used"], charged["model_calls_used"]), (1, 1))
        self.assertLess(charged["model_lease_ms_charged"], used["model_lease_ms_charged"])
        self.assertEqual(self.claim(), {"available": False})
        self.assertEqual(self.claim(claim_key=claim_key)["completed"], accepted)
        self.deny("IdempotencyConflict", "Model.ReportFailure", dict(request, outcome="Unknown"), self.provider)
        self.deny("Fenced", "Model.Respond", {"session_id": sid, "lease_id": first["lease_id"],
            "decision": {"action": "finish", "summary": "old lease"}}, self.provider)
        history = self.n.call("Session.History", {"id": sid}, self.owner["token"])
        self.assertEqual(history["turns"][0]["inference_failure"], accepted)
        self.assertIsNone(history["turns"][0]["decision"])

        self.n.stop(signal.SIGKILL)
        self.n.start()
        self.assertEqual(self.n.call("Model.ReportFailure", request, self.provider["token"]), accepted)
        resumed = self.n.call("Session.Resume", target(accepted["session_ref"]), self.owner["token"])
        self.assertEqual(resumed["session"]["lifecycle"], "AwaitingModel")
        second = self.claim()
        self.assertEqual((second["session_id"], second["turn"]), (sid, 2))
        self.assertNotEqual(second["lease_id"], first["lease_id"])
        finished = self.n.call("Model.Respond", {"session_id": sid, "lease_id": second["lease_id"],
            "decision": {"action": "finish", "summary": "after explicit resume"}}, self.provider["token"])
        self.assertEqual(finished["lifecycle"], "Succeeded")
        self.assertEqual(self.budget()["model_calls_used"], 2)

        one_call = self.grant(calls=1)
        limited = self.open(grant=one_call)
        lease = self.claim(limited)
        paused = self.n.call("Model.ReportFailure", self.report(limited, lease, "NotStarted"), self.provider["token"])
        self.deny("QuotaExceeded", "Session.Resume", target(paused["session_ref"]), self.owner)
        self.assertEqual(self.get(limited)["reference"], paused["session_ref"])

        revoked = self.grant()
        revoked_session = self.open(queued=False, grant=revoked)
        issued = self.claim(revoked_session)
        self.n.call("Grant.Revoke", target(revoked["grant_ref"]))
        paused = self.n.call("Model.ReportFailure", self.report(revoked_session, issued, "NotStarted"), self.provider["token"])
        self.deny("PermissionDenied", "Session.Resume", target(paused["session_ref"]), self.owner)
        self.verify()

    def test_unknown_blocks_fallback_and_keeps_provider_slot_after_cancellation(self):
        self.model = self.register(lease_seconds=3)
        self.authorization = self.grant()
        sid, waiting = self.open(), self.open()
        first = self.claim()
        self.assertEqual(first["session_id"], sid)
        fallback = {**target(self.get(sid)["reference"]), "model_ref": self.alternate}
        self.deny("PreconditionFailed", "Session.UseFallback", fallback, self.owner)
        spent = self.budget()
        request = self.report(sid, first, "Unknown")
        paused = self.n.call("Model.ReportFailure", request, self.provider["token"])
        self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"))
        paused_target = target(paused["session_ref"])
        self.deny("PreconditionFailed", "Session.Resume", paused_target, self.owner)
        self.deny("PreconditionFailed", "Session.UseFallback", {**paused_target, "model_ref": self.alternate}, self.owner)
        self.deny("OutcomeUnknown", "Model.Claim", {"session_id": sid, "key": key()}, self.provider)
        self.assertEqual(self.budget()["model_lease_ms_charged"], spent["model_lease_ms_charged"])
        cancelled = self.n.call("Session.Cancel", paused_target, self.owner["token"])
        self.assertEqual((cancelled["session"]["lifecycle"], cancelled["session"]["outcome_knowledge"]), ("Cancelled", "Unknown"))
        self.assertEqual(self.claim(), {"available": False})
        self.sleep_until_expired(first)
        replacement = self.claim()
        self.assertEqual(replacement["session_id"], waiting)
        cancelled_live = self.n.call("Session.Cancel", target(self.get(waiting)["reference"]), self.owner["token"])
        self.assertEqual(cancelled_live["session"]["outcome_knowledge"], "Unknown")
        self.deny("Fenced", "Model.ReportFailure", self.report(waiting, replacement, "NotStarted"), self.provider)
        self.assertEqual(self.n.call("Model.ReportFailure", request, self.provider["token"]), paused)
        self.verify()

    def sleep_until_expired(self, claim):
        limit = datetime.fromisoformat(claim["expires_at"])
        time.sleep(max(0, (limit - datetime.now(timezone.utc)).total_seconds()) + 0.1)

    def test_expiry_by_direct_claim_queue_and_watchdog_never_reissues(self):
        self.model = self.register(lease_seconds=2, max_in_flight=3)
        self.authorization = self.grant()
        manual, queued = self.open(queued=False), self.open()
        manual_key, queued_key = key(), key()
        # Keep expiry observable at the API, even if the periodic timer runs first.
        with sqlite3.connect(self.n.state / "state.db", timeout=10) as db:
            db.execute("""CREATE TRIGGER hold_model_watchdog BEFORE INSERT ON audit
                WHEN NEW.method='Model.LeaseExpired' AND NEW.request='model-expiry'
                BEGIN SELECT RAISE(ABORT,'hold only watchdog settlement'); END""")
        first = self.claim(manual, manual_key)
        queued_first = self.claim(claim_key=queued_key)
        self.assertEqual(queued_first["session_id"], queued)
        eligible = self.open()
        self.sleep_until_expired(queued_first)
        self.deny("OutcomeUnknown", "Model.Claim", {"session_id": manual, "key": key()}, self.provider)
        self.assertEqual(self.get(manual)["session"]["outcome_knowledge"], "Unknown")
        self.assertEqual(self.claim(manual, manual_key)["completed"]["lease_id"], first["lease_id"])
        unrelated = self.claim()
        self.assertEqual(unrelated["session_id"], eligible)
        self.assertEqual(self.claim(claim_key=queued_key)["completed"]["outcome"], "Unknown")
        self.deny("Fenced", "Model.ReportFailure", self.report(queued, queued_first, "NotStarted"), self.provider)
        with sqlite3.connect(self.n.state / "state.db", timeout=10) as db:
            db.execute("DROP TRIGGER hold_model_watchdog")
        self.n.stop(signal.SIGKILL)
        self.sleep_until_expired(unrelated)
        self.n.start()
        deadline = time.monotonic() + 5
        while self.get(eligible)["session"]["lifecycle"] != "Paused":
            self.assertLess(time.monotonic(), deadline, "watchdog failed to settle the old lease after restart")
            time.sleep(0.05)
        self.assertEqual(self.get(eligible)["session"]["outcome_knowledge"], "Unknown")
        self.assertEqual(self.budget()["model_calls_used"], 3)
        self.assertEqual(self.claim(), {"available": False})
        self.verify()

    def test_concurrent_decision_and_transport_report_have_one_settlement(self):
        sid = self.open(queued=False)
        lease = self.claim(sid)
        report = self.report(sid, lease, "Unknown")
        response = {"session_id": sid, "lease_id": lease["lease_id"],
                    "decision": {"action": "finish", "summary": "completed once"}}

        def attempt(method, params):
            try:
                return self.n.call(method, params, self.provider["token"])
            except Failure as failure:
                return failure.code

        with ThreadPoolExecutor(2) as concurrent:
            decision = concurrent.submit(attempt, "Model.Respond", response)
            failed = concurrent.submit(attempt, "Model.ReportFailure", report)
            results = [decision.result(), failed.result()]
        self.assertEqual(results.count("Fenced"), 1, results)
        accepted, = [receipt for receipt in results if isinstance(receipt, dict)]
        self.assertEqual(accepted["session_ref"], self.get(sid)["reference"])
        self.assertEqual(self.budget()["model_calls_used"], 1)
        self.verify()

    def test_verify_rejects_later_revisions_with_matching_lease_and_receipt(self):
        sid = self.open(queued=False)
        lease = self.claim(sid)
        after_issue = self.n.call("Session.Extend", {
            **target(self.get(sid)["reference"]), "max_turns": 5,
        }, self.owner["token"])["reference"]
        settled = self.n.call("Model.ReportFailure", self.report(sid, lease, "NotStarted"), self.provider["token"])
        after_settlement = self.n.call("Session.Extend", {
            **target(settled["session_ref"]), "max_turns": 6,
        }, self.owner["token"])["reference"]
        self.verify()

        def refusal(message):
            result = subprocess.run(self.n.daemon + ["verify"], capture_output=True, timeout=15)
            self.assertNotEqual(result.returncode, 0, result)
            self.assertIn(message, result.stderr)

        path = self.n.state / "state.db"
        ident = lease["lease_id"]
        with sqlite3.connect(path) as db:
            issued_rev, receipt = db.execute("SELECT session_revision,receipt FROM model_inference_leases "
                                             "WHERE lease=?", (ident,)).fetchone()
            db.execute("UPDATE model_inference_leases SET session_revision=? WHERE lease=?",
                       (int(after_issue["version"][1:]), ident))
        refusal(b"strict model dispatch checkpoint disagrees with its lease")
        with sqlite3.connect(path) as db:
            db.execute("UPDATE model_inference_leases SET session_revision=?,receipt=json_set(receipt,'$.session_ref',json(?)) "
                       "WHERE lease=?", (issued_rev, json.dumps(after_settlement), ident))
        refusal(b"strict model failure receipt disagrees with its settlement")
        with sqlite3.connect(path) as db:
            db.execute("UPDATE model_inference_leases SET receipt=? WHERE lease=?", (receipt, ident))
        self.verify()

