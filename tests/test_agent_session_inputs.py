"""Transfer exact private JSON to an authorized model provider through the real Unix API."""

from concurrent.futures import ThreadPoolExecutor
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
QUEUE_PRIOR_BINARIES = os.getenv("PSIOS_QUEUED_SESSION_INPUT_PRIOR_BIN_DIR")


class SessionInputFixture:
    bin_dir = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")

    def setUp(self):
        self.n = Node(self.bin_dir)
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

    def queued(self, claim_key, *, accept=None, wait_ms=None, provider=None):
        params = {"model_ref": self.model, "key": claim_key}
        if accept is not None:
            params["accepts_session_inputs"] = accept
        if wait_ms is not None:
            params["timeout_ms"] = wait_ms
        return self.n.call("Model.ClaimNext" if wait_ms is None else "Model.WaitNext", params,
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

    def session(self, reference):
        return self.n.call("Session.Get", {"id": reference["resource_id"]}, self.agent["token"])

    def model_calls(self, grant):
        return self.n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]},
                           self.agent["token"])["model_calls_used"]


class AgentSessionInputs(SessionInputFixture, unittest.TestCase):
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

    def test_task_retry_checks_both_session_limits_before_reacquiring_input(self):
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
        session = self.opened(self.opening(scoped, skill_ref=skill, provider_queue=True))
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
        current = self.session(session)
        self.assertEqual(current["session"]["lifecycle"], "Failed", current)

        params = {**target(current["reference"]), "task_id": response["task_id"], "key": key()}
        budget_id = scoped["budget_ref"]["resource_id"]
        session_id = session["resource_id"]

        def admission_state():
            with closing(sqlite3.connect(n.state / "state.db")) as conn:
                children = conn.execute(
                    "SELECT task FROM session_tasks WHERE session=? ORDER BY turn, attempt", (session_id,)
                ).fetchall()
                holds, = conn.execute(
                    "SELECT count(*) FROM session_input_holds WHERE session=?", (session_id,)
                ).fetchone()
            return children, holds, n.call("Budget.Get", {"id": budget_id}, token)

        def provider_queue_size():
            with closing(sqlite3.connect(n.state / "state.db")) as conn:
                count, = conn.execute(
                    "SELECT count(*) FROM model_queue WHERE model=?", (self.model["resource_id"],)
                ).fetchone()
            return count

        before = admission_state()
        self.assertEqual(before[:2], ([(response["task_id"],)], 0))
        self.assertEqual(provider_queue_size(), 0)
        owner_opening = self.opening(scoped, skill_ref=skill, input_artifacts=[])
        manual = [self.opened({**owner_opening, "key": key()}) for _ in range(32)]
        self.refuse("QuotaExceeded", "Session.RetryTask", params, token)
        self.assertEqual(self.session(session), current)
        self.assertEqual(admission_state(), before)
        n.call("Session.Cancel", target(manual.pop()), token)

        queued = []
        for _ in range(4):
            peer, peer_grant = n.agent(["session.run", "task.submit", "task.read", "node.inventory"])
            opening = self.opening(peer_grant, input_artifacts=[], provider_queue=True)
            for _ in range(32):
                ref = n.call("Session.Open", {**opening, "key": key()}, peer["token"])["reference"]
                queued.append((ref, peer["token"]))
        self.assertEqual(provider_queue_size(), 128)
        self.refuse("QueueFull", "Session.RetryTask", params, token)
        self.assertEqual(self.session(session), current)
        self.assertEqual(admission_state(), before)
        ref, peer_token = queued[-1]
        n.call("Session.Cancel", target(ref), peer_token)
        self.assertEqual(provider_queue_size(), 127)

        with ThreadPoolExecutor(max_workers=2) as executor:
            futures = [executor.submit(n.call, "Session.RetryTask", params, token) for _ in range(2)]
            accepted, replay = sorted((future.result() for future in futures), key=lambda x: x["deduplicated"])
        self.assertFalse(accepted["deduplicated"])
        self.assertEqual(replay, {**accepted, "deduplicated": True})
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), token)
        settled = n.wait(accepted["task_id"])
        self.assertEqual(settled["lifecycle"], "Succeeded", settled)
        self.assertEqual(provider_queue_size(), 128)
        after = admission_state()
        self.assertEqual(after[:2], ([(response["task_id"],), (accepted["task_id"],)], 1))
        self.assertEqual(after[2]["spent"], before[2]["spent"] + 1)
        self.assertEqual(after[2]["reserved"], before[2]["reserved"])
        self.assertEqual(after[2]["model_calls_used"], before[2]["model_calls_used"])
        self.assertEqual(n.call("Session.RetryTask", params, token), {**accepted, "deduplicated": True})
        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
        n.start()
        self.assertEqual(provider_queue_size(), 128)
        self.assertEqual(n.call("Session.RetryTask", params, token), {**accepted, "deduplicated": True})
        self.assertEqual(admission_state(), after)
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
        self.refuse("UnsupportedCapability", "Session.Open",
                    self.opening(owned, provider_queue=True, fallback_model_refs=[limited]), token)
        self.assertEqual(n.call("Budget.Get", {"id": owned["budget_ref"]["resource_id"]},
                                token)["model_calls_used"], 0)
        n.call("Artifact.Withdraw", self.withdrawal(), token)

    def test_queued_private_input_capability_orders_claims_and_preserves_exact_replay(self):
        n = self.n
        caller = self.agent["token"]
        grant = self.authorize()
        attached = self.opened(self.opening(grant, provider_queue=True))
        self.assertEqual(self.queued("settle-ordinary"), {"available": False})
        self.assertEqual(self.queued("wait-ordinary", accept=False, wait_ms=30), {"available": False})
        self.assertEqual(self.session(attached)["reference"], attached)
        self.assertEqual(self.model_calls(grant), 0)
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), caller)

        ordinary_grant = self.authorize()
        ordinary = self.opened(self.opening(ordinary_grant, provider_queue=True, input_artifacts=[]))
        legacy = self.queued("settle-ordinary")
        self.assertEqual(legacy["session_id"], ordinary["resource_id"])
        self.assertNotIn("inputs", legacy["context"])
        self.assertEqual(self.queued("settle-ordinary", accept=True), {**legacy, "recovered": True})
        legacy_receipt = self.respond(ordinary, legacy, {"action": "finish", "summary": "Queue ready."})
        self.assertEqual(legacy_receipt["lifecycle"], "Succeeded")
        self.assertEqual(self.session(attached)["reference"], attached)
        self.assertEqual(self.model_calls(grant), 0)
        outsider, _ = n.agent(["model.serve"])
        self.refuse("PermissionDenied", "Model.ClaimNext",
                    {"model_ref": self.model, "key": "foreign", "accepts_session_inputs": True}, outsider["token"])

        first = self.queued("use-exact-input", accept=True, wait_ms=1000)
        self.assertEqual(first["session_id"], attached["resource_id"])
        self.assertEqual(first["context"]["inputs"], [
            {"name": "approved-input", "reference": self.artifact, "kind": "Json", "value": self.data},
        ])
        status = {"model_ref": self.model, "key": "use-exact-input", "lease_id": first["lease_id"]}
        self.assertEqual(n.call("Model.ClaimStatus", status, self.provider["token"]), {"status": "Pending"})
        self.assertEqual(self.model_calls(grant), 1)
        next_ordinary = self.opened(self.opening(ordinary_grant, provider_queue=True, input_artifacts=[]))
        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
        n.start()
        self.refuse("UnsupportedCapability", "Model.WaitNext",
                    {"model_ref": self.model, "key": "use-exact-input", "timeout_ms": 1000}, self.provider["token"])
        self.assertEqual(self.session(next_ordinary)["reference"], next_ordinary)
        recovered = self.queued("use-exact-input", accept=True)
        self.assertEqual(recovered, {**first, "recovered": True})
        self.assertEqual(self.model_calls(grant), 1)

        work = {"action": "tool", "tool": recovered["context"]["inputs"][0]["value"]["tool"], "input": {}}
        receipt = self.respond(attached, recovered, work)
        self.assertEqual(self.respond(attached, first, work), receipt)
        completed = n.wait(receipt["task_id"])
        self.assertEqual(completed["lifecycle"], "Succeeded")
        result = n.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]}, caller)
        self.assertGreater(result["memory_bytes"]["MemTotal"], 0)
        later = self.queued("next-ordinary")
        self.assertEqual(later["session_id"], next_ordinary["resource_id"])
        self.assertNotIn("inputs", later["context"])
        self.respond(next_ordinary, later, {"action": "finish", "summary": "Queue still ready."})

        second = self.queued("finish-input-work", accept=True, wait_ms=3000)
        self.assertEqual(second["session_id"], attached["resource_id"])
        self.assertEqual(second["context"]["inputs"], first["context"]["inputs"])
        self.respond(attached, second, {"action": "finish", "summary": "Inventory recorded."})
        n.call("Artifact.Withdraw", self.withdrawal(), caller)
        completed_replay = self.queued("use-exact-input", accept=False)
        self.assertEqual(completed_replay, {"available": True, "session_id": attached["resource_id"],
                                           "turn": first["turn"], "completed": receipt})
        self.assertEqual(n.call("Model.ClaimStatus", status, self.provider["token"]), {"status": "Settled"})
        history = n.call("Session.History", {"id": attached["resource_id"]}, caller)
        self.assertEqual(len(history["tasks"]), 1)
        self.assertEqual([turn["context_digest"] for turn in history["turns"]],
                         [first["context_digest"], second["context_digest"]])
        self.assertNotIn(self.data["private_ticket"], json.dumps(history))
        self.assertEqual(self.model_calls(grant), 2)
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)

    def test_queued_provider_capability_never_replaces_original_disclosure_grant(self):
        n = self.n
        grant = self.authorize()
        revoked = self.opened(self.opening(grant, provider_queue=True))
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        next_grant = self.authorize()
        ordinary = self.opened(self.opening(next_grant, provider_queue=True, input_artifacts=[]))
        first = self.queued("legacy-worker")
        self.assertEqual(first["session_id"], ordinary["resource_id"])
        self.assertEqual(self.session(revoked)["reference"], revoked)
        self.respond(ordinary, first, {"action": "finish", "summary": "No input requested."})

        self.assertEqual(self.queued("authorized-worker", accept=True), {"available": False})
        parked = self.session(revoked)
        self.assertEqual(parked["session"]["lifecycle"], "Paused")
        self.assertEqual(parked["session"]["pause_reason"], "ModelAuthorizationDenied")
        self.assertEqual(parked["session"]["turn"], 0)
        n.call("Session.Cancel", target(parked["reference"]), self.agent["token"])

        active = self.opened(self.opening(next_grant, provider_queue=True))
        lease = self.queued("authorized-worker", accept=True)
        self.assertEqual(lease["session_id"], active["resource_id"])
        n.call("Grant.Revoke", target(next_grant["grant_ref"]))
        self.authorize()
        request = {"model_ref": self.model, "key": "authorized-worker", "accepts_session_inputs": True}
        self.refuse("PermissionDenied", "Model.ClaimNext", request, self.provider["token"])
        response = {"session_id": active["resource_id"], "lease_id": lease["lease_id"],
                    "decision": {"action": "tool", "tool": "node.inventory", "input": {}}}
        self.refuse("PermissionDenied", "Model.Respond", response, self.provider["token"])
        history = n.call("Session.History", {"id": active["resource_id"]}, self.agent["token"])
        self.assertEqual(history["tasks"], [])
        self.assertEqual(len(history["turns"]), 1)


@unittest.skipUnless(QUEUE_PRIOR_BINARIES, "requires the verified prior Core without private queue support")
class QueuedSessionInputUpgrade(SessionInputFixture, unittest.TestCase):
    bin_dir = QUEUE_PRIOR_BINARIES

    def test_old_writer_refuses_the_queue_disclosure_format(self):
        n = self.n
        grant = self.authorize()
        request = self.opening(grant, provider_queue=True)
        self.refuse("UnsupportedCapability", "Session.Open", request, self.agent["token"])
        ordinary = self.opened(self.opening(grant, provider_queue=True, input_artifacts=[]))
        old_claim = self.queued("pre-upgrade-ordinary")
        self.assertEqual(old_claim["session_id"], ordinary["resource_id"])
        self.assertNotIn("inputs", old_claim["context"])
        old_daemon = n.daemon[:]
        n.stop(signal.SIGKILL)
        source = json.loads(subprocess.check_output(old_daemon + ["verify"], timeout=20))
        self.assertIn(source["database_format"], (102, 103))

        n.daemon[0] = os.path.abspath(os.path.join(SessionInputFixture.bin_dir, "psiosd"))
        pending = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertTrue(pending["migration_required"])
        n.start()
        self.assertEqual(self.queued("pre-upgrade-ordinary"), {**old_claim, "recovered": True})
        self.respond(ordinary, old_claim, {"action": "finish", "summary": "Old queued turn settled."})
        attached = self.opened(request)
        self.assertEqual(self.queued("new-format-unnegotiated"), {"available": False})
        n.stop(signal.SIGKILL)
        old_writer = subprocess.run(old_daemon + ["serve"], capture_output=True, timeout=20, text=True)
        self.assertNotEqual(old_writer.returncode, 0, old_writer)
        self.assertIn("SchemaVersionUnsupported", old_writer.stdout + old_writer.stderr)
        upgraded = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertFalse(upgraded["migration_required"])
        self.assertGreaterEqual(upgraded["database_format"], 104)
        self.assertEqual(upgraded["node_id"], source["node_id"])
        n.start()
        first = self.queued("new-format-negotiated", accept=True)
        self.assertEqual(first["session_id"], attached["resource_id"])
        self.assertEqual(first["context"]["inputs"][0]["value"], self.data)


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
