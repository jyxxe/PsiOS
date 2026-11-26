"""Transfer exact private JSON to an authorized model provider through the real Unix API."""

from contextlib import closing
import json
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target
from test_artifact_ingress import chunk, upload_request


PRIOR_BINARIES = os.getenv("PSIOS_SESSION_INPUT_PRIOR_BIN_DIR")


class AgentSessionInputs(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.node_id = self.n.call("Node.Get", {})["reference"]["resource_id"]
        self.agent = self.n.call("Agent.Register", {"name": "private-input-owner"})
        self.grant(self.agent, ["artifact.upload", "artifact.read", "artifact.withdraw", "task.read"], [self.node_id])
        self.provider, _ = self.n.agent(["model.serve"])
        self.model = self.register_model()
        self.skill = self.n.call("Skill.Publish", {"name": "data-dependent-inventory", "version": "1",
            "license": "0BSD", "source": "local-test", "tools": ["node.inventory"],
            "instructions": "Choose the allowed tool named by the supplied JSON input."})
        self.data = {"tool": "node.inventory", "private_ticket": "scope-specific-opaque-sample"}
        self.artifact = self.upload(json.dumps(self.data).encode(), self.agent)

    def register_model(self, context_bytes=16384):
        return self.n.call("Model.Register", {"name": "session-input-provider", "revision": key(),
            "provider_principal_id": self.provider["principal_id"], "inference_outcome_policy": "Strict",
            "lease_seconds": 60, "max_in_flight": 4, "max_context_bytes": context_bytes,
            "max_output_bytes": 2048})

    def grant(self, agent, actions, targets):
        scope = {} if targets is None else {"tool_targets": sorted(set(targets))}
        return self.n.call("Grant.Issue", {"principal_id": agent["principal_id"], "actions": actions,
            "units": 8, "model_calls": 8, "ttl_seconds": 300, **scope})

    def authorize(self, artifact=None, model=None, *, omit=(), actions=None, wildcard=False):
        artifact = artifact or self.artifact
        model = model or self.model
        targets = [self.node_id, "profile-local", model["resource_id"], artifact["resource_id"]]
        return self.grant(self.agent, actions or [
            "session.run", "session.context.read", "task.submit", "task.read", "node.inventory", "artifact.read",
        ], None if wildcard else [value for value in targets if value not in omit])

    def upload(self, payload, agent, media="application/json"):
        token = agent["token"]
        stage = self.n.call("Artifact.Stage", upload_request(payload, declared_media_type=media), token)
        self.n.call("Artifact.Stage.PutChunk", chunk(stage, 0, payload), token)
        return self.n.call("Artifact.Publish", {"stage_id": stage["stage_id"]}, token)["artifact_ref"]

    def opening(self, grant, artifact=None, model=None, **overrides):
        return {"key": key(), "goal": "Read the allowed local data identified by the input.", "skill_ref": self.skill,
                "model_ref": model or self.model, "context_policy": "ReferencesOnly", "max_turns": 3,
                "delegation_id": grant["delegation_ref"]["resource_id"],
                "input_artifacts": [{"name": "approved-input", "reference": artifact or self.artifact, "kind": "Json"}],
                **overrides}

    def opened(self, body):
        return self.n.call("Session.Open", body, self.agent["token"])["reference"]

    def claimed(self, session, claim_key, provider=None, *, accept=True):
        support = {"accepts_session_inputs": True} if accept else {}
        return self.n.call("Model.Claim", {"session_id": session["resource_id"], "key": claim_key, **support},
                           (provider or self.provider)["token"])

    def respond(self, session, claim, decision):
        return self.n.call("Model.Respond", {"session_id": session["resource_id"], "lease_id": claim["lease_id"],
                          "decision": decision}, self.provider["token"])

    def refuse(self, code, method, params, token=None):
        with self.assertRaises(Failure) as rejected:
            self.n.call(method, params, token)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    def withdrawal(self):
        return {"reference": self.artifact, "reason": "session closed"}

    def test_exact_context_survives_crash_and_runs_a_real_authorized_task(self):
        n = self.n
        grant = self.authorize()
        opening = self.opening(grant)
        session = self.opened(opening)
        self.assertEqual(self.opened(opening), session)
        outsider, _ = n.agent(["model.serve"])
        self.refuse("PermissionDenied", "Model.Claim", {"session_id": session["resource_id"], "key": "outside"},
                    outsider["token"])
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), self.agent["token"])
        self.refuse("UnsupportedCapability", "Model.Claim", {"session_id": session["resource_id"], "key": "first"},
                    self.provider["token"])
        self.assertEqual(n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]},
                                self.agent["token"])["model_calls_used"], 0)
        first = self.claimed(session, "first")
        self.assertEqual(first["context"]["inputs"], [
            {"name": "approved-input", "reference": self.artifact, "kind": "Json", "value": self.data},
        ])
        current = n.call("Session.Get", {"id": session["resource_id"]}, self.agent["token"])
        self.assertNotIn(self.data["private_ticket"], json.dumps(current))
        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
        payload_key = (self.artifact["resource_id"],)
        with closing(sqlite3.connect(n.state / "state.db")) as conn, conn:
            original, = conn.execute("SELECT payload FROM artifact_chunks WHERE artifact_id=? AND chunk_index=0",
                                     payload_key).fetchone()
            conn.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload)) WHERE artifact_id=?", payload_key)
        invalid = subprocess.run(n.daemon + ["verify"], capture_output=True, timeout=20)
        self.assertNotEqual(invalid.returncode, 0)
        with closing(sqlite3.connect(n.state / "state.db")) as conn, conn:
            conn.execute("UPDATE artifact_chunks SET payload=? WHERE artifact_id=?", (original, *payload_key))
        n.start()
        self.refuse("UnsupportedCapability", "Model.Claim", {"session_id": session["resource_id"], "key": "first"},
                    self.provider["token"])
        replay = self.claimed(session, "first")
        self.assertEqual(replay, {**first, "recovered": True})

        request = {"action": "tool", "tool": replay["context"]["inputs"][0]["value"]["tool"], "input": {}}
        result = self.respond(session, replay, request)
        completed = n.wait(result["task_id"])
        self.assertEqual(completed["lifecycle"], "Succeeded")
        report = n.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]}, self.agent["token"])
        self.assertGreater(report["memory_bytes"]["MemTotal"], 0)
        second = self.claimed(session, "second")
        self.assertEqual(second["context"]["inputs"], first["context"]["inputs"])
        final = self.respond(session, second, {"action": "finish", "summary": "Inventory recorded."})
        self.assertEqual(final["lifecycle"], "Succeeded")
        self.assertEqual(n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]},
                                self.agent["token"])["model_calls_used"], 2)
        history = n.call("Session.History", {"id": session["resource_id"]}, self.agent["token"])
        self.assertEqual([turn["context_digest"] for turn in history["turns"]],
                         [first["context_digest"], second["context_digest"]])
        self.assertEqual(len(history["tasks"]), 1)
        self.assertNotIn(self.data["private_ticket"], json.dumps(history))
        n.call("Artifact.Withdraw", self.withdrawal(), self.agent["token"])
        completed_replay = self.claimed(session, "first", accept=False)
        self.assertEqual(completed_replay["completed"], result)
        self.assertNotIn("context", completed_replay)
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)

    def test_original_disclosure_cannot_be_replaced_after_revocation(self):
        n = self.n
        grant = self.authorize()
        session = self.opened(self.opening(grant))
        first = self.claimed(session, "first")
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.authorize()
        n.stop(signal.SIGKILL)
        n.start()
        self.refuse("PermissionDenied", "Model.Claim",
                    {"session_id": session["resource_id"], "key": "first",
                     "accepts_session_inputs": True}, self.provider["token"])
        self.refuse("PermissionDenied", "Model.Respond",
                    {"session_id": session["resource_id"], "lease_id": first["lease_id"],
                     "decision": {"action": "tool", "tool": "node.inventory", "input": {}}}, self.provider["token"])
        history = n.call("Session.History", {"id": session["resource_id"]}, self.agent["token"])
        self.assertEqual(history["tasks"], [])
        self.assertEqual(len(history["turns"]), 1)
        current = n.call("Session.Get", {"id": session["resource_id"]}, self.agent["token"])
        n.call("Session.Cancel", target(current["reference"]), self.agent["token"])
        n.call("Artifact.Withdraw", self.withdrawal(), self.agent["token"])

    def test_task_retry_atomically_reacquires_the_original_input_hold(self):
        n = self.n
        token = self.agent["token"]
        new_volume = self.grant(self.agent, ["task.submit", "task.read", "volume.create"], None)
        request = n.prepare("volume.create", {"name": "session-input-retry-volume"}, token=token,
                            delegation=new_volume["delegation_ref"]["resource_id"])
        made = n.wait(n.call("Task.Submit", request, token)["task_id"])
        self.assertEqual(made["lifecycle"], "Succeeded", made)
        volume = n.call("Artifact.Read", {"reference": made["result"]["artifact_refs"][0]}, token)
        volume = n.perform("volume.write", {**target(volume), "name": "stable", "content": "ready"})["volume_ref"]
        skill = n.call("Skill.Publish", {"name": "read-retry-skill", "version": "1", "license": "0BSD",
            "source": "local-test", "tools": ["volume.read"], "instructions": "Read the allowed volume record."})
        scoped = self.grant(self.agent, ["session.run", "session.context.read", "artifact.read", "task.read",
                            "task.submit", "volume.read"],
                            [self.node_id, self.model["resource_id"], self.artifact["resource_id"],
                             volume["resource_id"]])
        session = self.opened(self.opening(scoped, skill_ref=skill))
        first = self.claimed(session, "before-host-failure")
        source = n.state / "volumes" / volume["resource_id"] / "data"
        displaced = source.with_name("temporarily-missing-data")
        source.rename(displaced)
        try:
            response = self.respond(session, first, {"action": "tool", "tool": "volume.read",
                                    "input": {**target(volume), "name": "stable"}})
            failed = n.wait(response["task_id"])
            self.assertEqual(failed["lifecycle"], "Failed", failed)
        finally:
            displaced.rename(source)
        current = n.call("Session.Get", {"id": session["resource_id"]}, token)
        self.assertEqual(current["session"]["lifecycle"], "Failed", current)
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            self.assertEqual(conn.execute("SELECT count(*) FROM session_input_holds WHERE session=?",
                             (session["resource_id"],)).fetchone(), (0,))

        params = {**target(current["reference"]), "task_id": response["task_id"], "key": key()}
        accepted = n.call("Session.RetryTask", params, token)
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), token)
        settled = n.wait(accepted["task_id"])
        self.assertEqual(settled["lifecycle"], "Succeeded", settled)
        self.assertEqual(n.call("Session.RetryTask", params, token), {**accepted, "deduplicated": True})
        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
        n.start()
        second = self.claimed(session, "after-exact-task-retry")
        self.assertEqual(second["context"]["inputs"], first["context"]["inputs"])
        final = self.respond(session, second, {"action": "finish", "summary": "Read after storage recovered."})
        self.assertEqual(final["lifecycle"], "Succeeded")
        n.call("Artifact.Withdraw", self.withdrawal(), token)

    def test_open_rejects_foreign_unscoped_untyped_and_excessive_inputs(self):
        n = self.n
        token = self.agent["token"]
        for scope in [self.artifact["resource_id"], self.model["resource_id"]]:
            params = self.opening(self.authorize(omit=[scope]))
            self.refuse("PermissionDenied", "Session.Open", params, token)
        with self.assertRaises(Failure) as unscoped_grant:
            self.authorize(wildcard=True)
        self.assertEqual(unscoped_grant.exception.code, "InvalidArgument")
        foreign = n.call("Agent.Register", {"name": "foreign-input-owner"})
        self.grant(foreign, ["artifact.upload"], [self.node_id])
        foreign_ref = self.upload(b'{"outside":true}', foreign)
        owned = self.authorize()
        self.refuse("PermissionDenied", "Session.Open",
                    self.opening(self.authorize(artifact=foreign_ref), artifact=foreign_ref), token)
        text_ref = self.upload(b'{"looks":"structured"}', self.agent, "text/plain")
        self.refuse("PermissionDenied", "Session.Open",
                    self.opening(self.authorize(artifact=text_ref), artifact=text_ref), token)
        ambiguous = self.upload(b'{"value":1,"value":2}', self.agent)
        self.refuse("InvalidArgument", "Session.Open",
                    self.opening(self.authorize(artifact=ambiguous), artifact=ambiguous), token)
        large = self.upload(json.dumps({"value": "x" * 17000}).encode(), self.agent)
        self.refuse("QuotaExceeded", "Session.Open",
                    self.opening(self.authorize(artifact=large), artifact=large), token)
        limited = self.register_model(context_bytes=1024)
        medium = self.upload(json.dumps({"value": "x" * 900}).encode(), self.agent)
        self.refuse("QuotaExceeded", "Session.Open",
                    self.opening(self.authorize(artifact=medium, model=limited), artifact=medium, model=limited), token)
        repeated = self.opening(owned)
        repeated["input_artifacts"] *= 2
        self.refuse("InvalidArgument", "Session.Open", repeated, token)
        self.refuse("UnsupportedCapability", "Session.Open", self.opening(owned, provider_queue=True), token)
        self.assertEqual(n.call("Budget.Get", {"id": owned["budget_ref"]["resource_id"]},
                                token)["model_calls_used"], 0)
        n.call("Artifact.Withdraw", self.withdrawal(), token)


@unittest.skipUnless(PRIOR_BINARIES, "requires the verified prior format-99 Core")
class ExistingSessionUpgrade(unittest.TestCase):
    def test_old_inflight_session_replays_exact_context_without_input_negotiation(self):
        n = Node(PRIOR_BINARIES)
        self.addCleanup(n.close)
        provider, _ = n.agent(["model.serve"])
        agent = n.call("Agent.Register", {"name": "legacy-session-owner"})
        grant = n.call("Grant.Issue", {"principal_id": agent["principal_id"],
            "actions": ["session.run", "task.read", "task.submit", "node.inventory"],
            "units": 2, "model_calls": 2, "ttl_seconds": 300})
        model = n.call("Model.Register", {"name": "replay-provider", "revision": "1",
            "provider_principal_id": provider["principal_id"], "inference_outcome_policy": "Strict",
            "lease_seconds": 60, "max_context_bytes": 4096, "max_output_bytes": 2048})
        skill = n.call("Skill.Publish", {"name": "replay-skill", "version": "1", "license": "0BSD",
            "source": "local-test", "tools": ["node.inventory"], "instructions": "Review the summary."})
        session = n.call("Session.Open", {"key": key(), "goal": "Review the pre-upgrade session.",
            "skill_ref": skill, "model_ref": model, "context_policy": "ReferencesOnly", "tool_context_window": 1,
            "max_turns": 2, "delegation_id": grant["delegation_ref"]["resource_id"]}, agent["token"])["reference"]
        request = {"session_id": session["resource_id"], "key": "before-storage-inputs"}
        first = n.call("Model.Claim", request, provider["token"])
        self.assertNotIn("inputs", first["context"])
        n.stop(signal.SIGKILL)
        old = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertEqual(old["database_format"], 99)

        n.daemon[0] = os.path.abspath(os.path.join(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"), "psiosd"))
        staged = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertTrue(staged["migration_required"])
        n.start()
        replay = n.call("Model.Claim", request, provider["token"])
        self.assertEqual(replay, {**first, "recovered": True})
        result = n.call("Model.Respond", {"session_id": session["resource_id"], "lease_id": replay["lease_id"],
            "decision": {"action": "finish", "summary": "Retained original request."}}, provider["token"])
        self.assertEqual(result["lifecycle"], "Succeeded")
        n.stop()
        current = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertGreaterEqual(current["database_format"], 100)
        self.assertEqual(current["node_id"], old["node_id"])
        self.assertFalse(current["migration_required"])
