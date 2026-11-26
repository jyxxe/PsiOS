"""Pre-authorized local model failover with a real Task checkpoint and independent providers."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target


def register_provider(node, name, *, strict=False):
    provider, _ = node.agent(["model.serve"])
    profile = {
        "name": name, "revision": "v1", "provider_principal_id": provider["principal_id"],
        "max_context_bytes": 131072, "max_output_bytes": 8192, "lease_seconds": 20, "max_in_flight": 1,
    }
    if strict:
        profile["inference_outcome_policy"] = "Strict"
    model = node.call("Model.Register", profile)
    return provider, model


def publish_skill(node):
    return node.call("Skill.Publish", {
        "name": "handoff-status", "version": "v1", "tools": ["system.status"],
        "instructions": "Report a verified local machine status.", "source": "local-test", "license": "0BSD",
    })


def submit_decision(node, provider, lease, decision):
    return node.call("Model.Respond", {
        "session_id": lease["session_id"], "lease_id": lease["lease_id"], "decision": decision,
    }, provider["token"])


class ModelFallbackTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.primary, self.primary_model = register_provider(self.n, "handoff-primary")
        self.alternate, self.alternate_model = register_provider(self.n, "handoff-alternate")
        self.skill = publish_skill(self.n)
        self.owner = self.n.call("Agent.Register", {"name": "handoff-session-owner"})
        self.actions = ["task.submit", "task.read", "session.run", "system.status", "agent.session.start"]
        self.grant = self.issue([self.primary_model, self.alternate_model])

    def issue(self, models, allow=True, *, calls=5):
        actions = self.actions + (["session.failover"] if allow else [])
        return self.n.call("Grant.Issue", {
            "principal_id": self.owner["principal_id"], "actions": actions,
            "tool_targets": [m["resource_id"] for m in models] + [self.skill["resource_id"], "profile-local"],
            "units": 8, "model_calls": calls, "model_lease_ms": 100_000, "ttl_seconds": 180,
        })

    def blueprint(self, **overrides):
        return {
            "goal": "Inspect local status", "skill_ref": self.skill, "model_ref": self.primary_model,
            "fallback_model_refs": [self.alternate_model], "context_policy": "ReferencesOnly",
            "max_turns": 4, **overrides,
        }

    def open(self, grant=None, **overrides):
        grant = grant or self.grant
        return self.n.call("Session.Open", {
            "key": key(), "provider_queue": True, "delegation_id": grant["delegation_ref"]["resource_id"],
            **self.blueprint(**overrides),
        }, self.owner["token"])["reference"]

    def observe(self, sid):
        return self.n.call("Session.Get", {"id": sid}, self.owner["token"])

    def claim(self, provider, model, claim_key=None):
        return self.n.call("Model.ClaimNext", {"model_ref": model, "key": claim_key or key()}, provider["token"])

    def denied(self, code, method, params, actor):
        with self.assertRaises(Failure) as e:
            self.n.call(method, params, actor["token"])
        self.assertEqual(e.exception.code, code, e.exception)

    def handoff_request(self, session, model=None):
        return {**target(session), "model_ref": model or self.alternate_model}

    def strict_pair(self):
        self.primary, self.primary_model = register_provider(self.n, "strict-handoff-primary", strict=True)
        self.alternate, self.alternate_model = register_provider(self.n, "strict-handoff-alternate", strict=True)
        self.grant = self.issue([self.primary_model, self.alternate_model])

    def report(self, claim, outcome):
        return self.n.call("Model.ReportFailure", {"session_id": claim["session_id"],
            "model_ref": self.primary_model, "lease_id": claim["lease_id"], "outcome": outcome},
            self.primary["token"])

    def verify_stopped(self):
        result = subprocess.run(self.n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["sqlite_integrity"], "Verified")

    def test_witnessed_not_started_can_handoff_after_primary_retirement(self):
        self.strict_pair()
        n = self.n
        session = self.open()["resource_id"]
        first = self.claim(self.primary, self.primary_model)
        result = submit_decision(n, self.primary, first, {"action": "tool", "tool": "system.status", "input": {}})
        written = n.wait(result["task_id"])
        self.assertEqual((written["lifecycle"], written["outcome_knowledge"]), ("Succeeded", "Known"), written)
        artifact, = written["result"]["artifact_refs"]
        claim = self.claim(self.primary, self.primary_model)
        self.assertEqual(claim["turn"], 2)
        self.assertEqual(claim["context"]["last_tool"]["task_id"], result["task_id"])
        self.denied("PreconditionFailed", "Session.UseFallback",
                    self.handoff_request(self.observe(session)["reference"]), self.owner)
        report = self.report(claim, "NotStarted")
        paused = self.observe(session)
        self.assertEqual(report["session_ref"], paused["reference"])
        self.assertEqual(paused["session"]["model_inference_failure"]["lease_id"], claim["lease_id"])
        extended = n.call("Session.Extend", {**target(paused["reference"]), "max_turns": 5}, self.owner["token"])
        self.assertEqual(extended["session"]["pause_reason"], "ModelNotStarted")
        n.call("Model.Retire", target(self.primary_model))
        self.denied("PreconditionFailed", "Session.Resume", target(extended["reference"]), self.owner)
        self.denied("Conflict", "Session.UseFallback", self.handoff_request(paused["reference"]), self.owner)
        self.denied("PermissionDenied", "Session.UseFallback", self.handoff_request(extended["reference"]),
                    self.alternate)

        budget_params = {"id": self.grant["budget_ref"]["resource_id"]}
        before = n.call("Budget.Get", budget_params)
        request = self.handoff_request(extended["reference"])
        accepted = n.call("Session.UseFallback", request, self.owner["token"])
        successor = accepted["successor"]["reference"]["resource_id"]
        self.assertNotEqual(successor, session)
        state = accepted["successor"]["session"]
        self.assertEqual((state["lifecycle"], state["turn"], state["max_turns"]), ("AwaitingModel", 2, 5))
        self.assertEqual(state["model_ref"], self.alternate_model)
        self.assertEqual(state["budget_ref"], self.grant["budget_ref"])
        self.assertNotIn("model_inference_failure", state)
        self.assertNotIn("pause_reason", state)
        self.assertEqual(self.observe(session)["session"]["model_inference_failure"],
                         paused["session"]["model_inference_failure"])
        self.assertEqual(n.call("Budget.Get", budget_params), before)

        n.stop(signal.SIGKILL)
        self.verify_stopped()
        self.reject_other_pause_revision(session, extended["reference"])
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Session.UseFallback", request, self.owner["token"]), accepted)
        fresh = self.claim(self.alternate, self.alternate_model)
        self.assertEqual((fresh["session_id"], fresh["turn"]), (successor, 3))
        self.assertNotEqual(fresh["lease_id"], claim["lease_id"])
        self.assertEqual(fresh["context"]["last_tool"]["result"]["artifact_refs"], [artifact])
        finished = submit_decision(n, self.alternate, fresh, {"action": "finish", "summary": "Verified local status"})
        self.assertEqual(finished["lifecycle"], "Succeeded")
        self.assertEqual(n.call("Budget.Get", budget_params)["model_calls_used"], before["model_calls_used"] + 1)
        history = n.call("Session.History", {"id": session}, self.owner["token"])
        self.assertEqual(history["turns"][1]["inference_failure"], report)
        self.assertEqual(n.call("Session.UseFallback", request, self.owner["token"]), accepted)
        n.stop()
        self.verify_stopped()

    def reject_other_pause_revision(self, session, replacement):
        path = self.n.state / "state.db"
        witness = f"model-handoff-{session}"
        update = "UPDATE versions SET body=? WHERE id=? AND rev=1"
        with closing(sqlite3.connect(path, timeout=10)) as db, db:
            raw, = db.execute("SELECT body FROM versions WHERE id=? AND rev=1", (witness,)).fetchone()
            body = json.loads(raw)
            self.assertNotEqual(body["not_started_session_ref"], replacement)
            body["not_started_session_ref"] = replacement
            db.execute(update, (json.dumps(body), witness))
        try:
            refused = subprocess.run(self.n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
            self.assertNotEqual(refused.returncode, 0, refused)
            self.assertIn("RecoveryRequired", refused.stderr)
            self.assertIn("model handoff lost its exact NotStarted lineage", refused.stderr)
        finally:
            with closing(sqlite3.connect(path, timeout=10)) as db, db:
                db.execute(update, (raw, witness))

    def test_not_started_handoff_refuses_exhausted_budget_and_revoked_owner_authority(self):
        self.strict_pair()
        n = self.n
        limited = self.issue([self.primary_model, self.alternate_model], calls=1)
        session = self.open(limited)["resource_id"]
        claim = self.claim(self.primary, self.primary_model)
        self.assertEqual(claim["session_id"], session)
        report = self.report(claim, "NotStarted")
        request = self.handoff_request(report["session_ref"])
        self.denied("QuotaExceeded", "Session.UseFallback", request, self.owner)
        self.assertEqual(self.observe(session)["reference"], report["session_ref"])
        self.assertFalse(self.claim(self.alternate, self.alternate_model)["available"])
        n.call("Grant.Revoke", target(limited["grant_ref"]))
        self.denied("PermissionDenied", "Session.UseFallback", request, self.owner)

        unknown = self.open()["resource_id"]
        live = self.claim(self.primary, self.primary_model)
        self.assertEqual(live["session_id"], unknown)
        failed = self.report(live, "Unknown")
        self.denied("PreconditionFailed", "Session.UseFallback", self.handoff_request(failed["session_ref"]),
                    self.owner)
        self.assertEqual(self.observe(unknown)["reference"], failed["session_ref"])
        self.assertFalse(self.claim(self.alternate, self.alternate_model)["available"])

    def test_handoff_preserves_verified_task_result_fences_processor_and_keeps_budget(self):
        n = self.n
        original = self.open()["resource_id"]
        first = self.claim(self.primary, self.primary_model)
        self.assertEqual(first["session_id"], original)
        done = submit_decision(n, self.primary, first, {"action": "tool", "tool": "system.status", "input": {}})
        task = done["task_id"]
        produced = n.wait(task)
        self.assertEqual(produced["lifecycle"], "Succeeded")
        ref, = produced["result"]["artifact_refs"]
        self.assertEqual(n.call("Artifact.Read", {"reference": ref})["authority_id"],
                         n.call("Principal.WhoAmI", {})["authority_id"])
        old_key = key()
        old = self.claim(self.primary, self.primary_model, old_key)
        self.assertEqual(old["turn"], 2)
        self.assertEqual(old["context"]["last_tool"]["task_id"], task)
        waiting = self.open(fallback_model_refs=[])["resource_id"]
        self.assertNotEqual(waiting, original)
        before = n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]})
        request = self.handoff_request(self.observe(original)["reference"])
        token_file = n.base / "handoff-owner.token"
        token_file.write_text(self.owner["token"])
        token_file.chmod(0o600)
        model_file = n.base / "alternate-model.json"
        model_file.write_text(json.dumps(self.alternate_model))
        changed = n.cli(token_file, "session-use-fallback", original, "--revision",
                        request["expected_resource_version"], model_file)
        successor = changed["successor"]["reference"]["resource_id"]
        self.assertNotEqual(successor, original)
        self.assertEqual(changed["successor"]["session"]["turn"], 2)
        self.assertEqual(changed["successor"]["session"]["fallback_model_refs"], [])
        self.assertEqual(changed["successor"]["session"]["last_task"], task)
        self.assertEqual(n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]}), before)
        self.assertEqual(self.observe(original)["session"]["successor_ref"], changed["successor"]["reference"])
        self.assertFalse(self.claim(self.primary, self.primary_model)["available"],
                         "a live abandoned provider lease still consumes its original slot")
        n.call("Model.Retire", target(self.primary_model))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Session.UseFallback", request, self.owner["token"]), changed)
        disposition = n.call("Model.ClaimStatus", {
            "model_ref": self.primary_model, "key": old_key, "lease_id": old["lease_id"],
        }, self.primary["token"])
        self.assertEqual(disposition, {"status": "Fenced"})
        self.denied("Fenced", "Model.Respond", {
            "session_id": original, "lease_id": old["lease_id"],
            "decision": {"action": "finish", "summary": "stale response"},
        }, self.primary)
        with self.assertRaises(Failure) as retained:
            n.call("Task.EvictResults", {**target(n.get(task)["reference"]), "reason": "check owner's active successor"})
        self.assertEqual(retained.exception.code, "Conflict")

        fresh = self.claim(self.alternate, self.alternate_model, old_key)
        self.assertEqual((fresh["session_id"], fresh["turn"]), (successor, 3))
        self.assertEqual(fresh["context"]["last_tool"]["result"]["artifact_refs"], [ref])
        self.assertNotIn("local_tool_result", fresh["context"]["last_tool"])
        self.denied("PermissionDenied", "Artifact.Read", {"reference": ref}, self.alternate)
        self.assertEqual(fresh["context"]["model_ref"], self.alternate_model)
        finished = submit_decision(n, self.alternate, fresh, {"action": "finish", "summary": "verified by alternate"})
        self.assertEqual(finished["lifecycle"], "Succeeded")
        self.assertEqual(self.observe(successor)["session"]["summary"], "verified by alternate")
        saved_budget = n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]})
        self.assertEqual(saved_budget["model_calls_used"], 3)
        self.assertGreaterEqual(saved_budget["model_lease_ms_charged"], before["model_lease_ms_charged"])
        n.call("Task.EvictResults", {**target(n.get(task)["reference"]), "reason": "successor finished"})
        original_history = n.call("Session.History", {"id": original}, self.owner["token"])
        successor_history = n.call("Session.History", {"id": successor}, self.owner["token"])
        self.assertEqual([turn["turn"] for turn in original_history["turns"]], [1, 2])
        self.assertEqual([turn["turn"] for turn in successor_history["turns"]], [3])

    def test_handoff_requires_both_model_scopes_and_refuses_unsettled_task(self):
        n = self.n
        _, third_model = register_provider(n, "unselected-third")
        self.denied("InvalidArgument", "Session.Open", {
            **self.blueprint(context_policy="LocalToolResults"), "key": key(),
            "delegation_id": self.grant["delegation_ref"]["resource_id"],
        }, self.owner)
        partial = self.issue([self.primary_model])
        with self.assertRaises(Failure) as target_scope:
            self.open(partial)
        self.assertEqual(target_scope.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as missing_action:
            self.open(self.issue([self.primary_model, self.alternate_model], allow=False))
        self.assertEqual(missing_action.exception.code, "PermissionDenied")

        authorized = self.issue([self.primary_model, self.alternate_model, third_model])
        request = n.prepare("agent.session.start", self.blueprint(), token=self.owner["token"],
                            delegation=authorized["delegation_ref"]["resource_id"])
        start_task = n.call("Task.Submit", request, self.owner["token"])["task_id"]
        source = n.wait(start_task)
        self.assertEqual(source["lifecycle"], "Succeeded", source)
        session = n.call("Artifact.Read", {"reference": source["result"]["artifact_refs"][0]})["session_ref"]
        self.denied("PermissionDenied", "Session.UseFallback", self.handoff_request(session, third_model), self.owner)
        self.denied("PermissionDenied", "Session.UseFallback", self.handoff_request(session), self.alternate)

        node = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
        turn = self.claim(self.primary, self.primary_model)
        child = submit_decision(n, self.primary, turn, {"action": "tool", "tool": "system.status", "input": {}})
        before = self.observe(turn["session_id"])
        self.assertEqual(before["session"]["lifecycle"], "WaitingTask")
        self.denied("PreconditionFailed", "Session.UseFallback", self.handoff_request(before["reference"]), self.owner)
        self.assertEqual(self.observe(turn["session_id"]), before)
        self.assertEqual(n.call("Task.Get", {"id": child["task_id"]})["lifecycle"], "Queued")
        self.assertFalse(self.claim(self.alternate, self.alternate_model)["available"])

    def test_model_decision_and_handoff_have_one_winner(self):
        n = self.n
        node = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
        session = self.open()["resource_id"]
        claim = self.claim(self.primary, self.primary_model)
        request = self.handoff_request(self.observe(session)["reference"])

        def handoff():
            try:
                return n.call("Session.UseFallback", request, self.owner["token"])
            except Failure as refused:
                self.assertEqual(refused.code, "Conflict", refused)
                return None

        def decide():
            try:
                return submit_decision(n, self.primary, claim,
                                       {"action": "tool", "tool": "system.status", "input": {}})
            except Failure as stale:
                self.assertEqual(stale.code, "Fenced", stale)
                return None

        with ThreadPoolExecutor(max_workers=2) as pool:
            model = pool.submit(decide)
            switch = pool.submit(handoff)
            decision, changed = model.result(), switch.result()
        self.assertNotEqual(decision is None, changed is None)
        if decision:
            self.assertEqual(n.call("Task.Get", {"id": decision["task_id"]})["lifecycle"], "Queued")
            self.assertEqual(self.observe(session)["session"]["lifecycle"], "WaitingTask")
            self.assertFalse(self.claim(self.alternate, self.alternate_model)["available"])
        else:
            turn = self.claim(self.alternate, self.alternate_model)
            self.assertEqual(turn["session_id"], changed["successor"]["reference"]["resource_id"])
            self.assertIsNone(turn["context"]["last_tool"])
            history = n.call("Session.History", {"id": session}, self.owner["token"])
            self.assertFalse(history["tasks"])
