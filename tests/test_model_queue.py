"""Provider work is delivered through the real Unix API and daemon."""
import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from datetime import datetime, timedelta, timezone
import json
import signal
import socket
import sqlite3
import struct
import subprocess
import time
import unittest

from model_delivery import frame, interrupt_worker_delivery
from support import Failure, Node, key, target


class ModelQueueTests(unittest.TestCase):
    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)
        self.tools = ["node.inventory", "node.devices"]
        self.skill = self.n.call("Skill.Publish", dict(name="inventory", version="1", tools=self.tools,
            source="local-test", license="0BSD", instructions="Read node inventory and count block devices."))
        self.provider, self.provider_grant = self.n.agent(["model.serve"])
        self.model = self.register_model()
        self.token_file = self.n.base / "provider.token"
        self.token_file.write_text(self.provider["token"])
        self.token_file.chmod(0o600)
        self.model_file = self.n.base / "model.json"
        self.model_file.write_text(json.dumps(self.model))
        self.journal = self.n.base / "decisions"
        self.journal.mkdir(mode=0o700)
        self.agent, self.grant = self.new_agent()

    def register_model(self, **kwargs):
        return self.n.call("Model.Register", dict(dict(name="survey", revision=key(), lease_seconds=20,
            provider_principal_id=self.provider["principal_id"], max_in_flight=1, max_context_bytes=131072,
            max_output_bytes=8192), **kwargs))

    def new_agent(self, calls=12):
        agent = self.n.call("Agent.Register", {"name": "observer"})
        grant = self.n.call("Grant.Issue", dict(principal_id=agent["principal_id"], units=12, model_calls=calls,
            actions=self.tools + ["task.submit", "task.read", "session.run"], ttl_seconds=180))
        return agent, grant

    def open(self, agent=None, grant=None, **kwargs):
        agent, grant = agent or self.agent, grant or self.grant
        params = dict(dict(key=key(), goal="Inspect devices", skill_ref=self.skill, model_ref=self.model,
            delegation_id=grant["delegation_ref"]["resource_id"], context_policy="LocalToolResults",
            max_turns=4, provider_queue=True), **kwargs)
        return self.n.call("Session.Open", params, agent["token"])["reference"]

    def next_turn(self, claim_key=None, token=None, timeout_ms=None, model=None):
        params = dict(model_ref=model or self.model, key=claim_key or key())
        method = "Model.ClaimNext"
        if timeout_ms is not None:
            method = "Model.WaitNext"
            params["timeout_ms"] = timeout_ms
        return self.n.call(method, params, token or self.provider["token"])

    def finish(self, claim):
        return self.n.call("Model.Respond", dict(session_id=claim["session_id"], lease_id=claim["lease_id"],
            decision=dict(action="finish", summary="complete")), self.provider["token"])

    def status(self, ref, agent=None):
        return self.n.call("Session.Get", {"id": ref["resource_id"]}, (agent or self.agent)["token"])

    def worker(self, *args):
        return ["model-worker", self.model_file, "--decision-dir", self.journal, "--wait-ms", "300", *args]

    def blueprint(self):
        return dict(goal="Inspect this node", skill_ref=self.skill, model_ref=self.model,
                    context_policy="LocalToolResults", max_turns=3)

    def grant_for_start(self, *actions, tool_targets=None):
        grant = dict(principal_id=self.agent["principal_id"], units=3, model_calls=3, ttl_seconds=180,
                     actions=["agent.session.start", "task.submit", "task.read", *actions])
        if tool_targets is not None:
            grant["tool_targets"] = tool_targets
        return self.n.call("Grant.Issue", grant)

    def test_identity_queue_order_and_concurrent_replay(self):
        manual = self.open(provider_queue=False)
        first = self.open()
        second = self.open()
        other, _ = self.n.agent(["model.serve"])
        with self.assertRaises(Failure) as denied:
            self.next_turn("same", other["token"])
        self.assertEqual(denied.exception.code, "PermissionDenied")
        other_model = self.register_model()
        self.assertEqual(self.next_turn("same", model=other_model), {"available": False})
        with ThreadPoolExecutor(2) as workers:
            claims = list(workers.map(self.next_turn, ["same", "same"]))
        self.assertEqual({c["session_id"] for c in claims}, {first["resource_id"]})
        self.assertEqual(len({c["lease_id"] for c in claims}), 1)
        self.assertEqual(sorted(c["recovered"] for c in claims), [False, True])
        self.assertEqual(self.next_turn(), {"available": False})
        receipt = self.finish(claims[0])
        self.n.stop(signal.SIGKILL)
        self.n.start()
        self.assertEqual(self.next_turn("same")["completed"], receipt)
        next_claim = self.next_turn()
        self.assertEqual(next_claim["session_id"], second["resource_id"])
        self.finish(next_claim)
        self.assertEqual(self.next_turn(timeout_ms=125), {"available": False})
        self.assertEqual(self.status(manual)["session"]["turn"], 0)
        self.assertEqual(self.n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]},
            self.agent["token"])["model_calls_used"], 2)

    def test_missing_tool_context_pauses_one_session_and_restores_the_original_result(self):
        n = self.n
        session = self.open()
        initial = self.next_turn("observe-once")
        self.assertEqual(initial["session_id"], session["resource_id"])
        decision = n.call("Model.Respond", {
            "session_id": initial["session_id"], "lease_id": initial["lease_id"],
            "decision": {"action": "tool", "tool": "node.inventory", "input": {}},
        }, self.provider["token"])
        tid = decision["task_id"]
        tool = n.wait(tid)
        reference, = tool["result"]["artifact_refs"]
        original = n.call("Artifact.Read", {"reference": reference}, self.agent["token"])

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            parts = db.execute("SELECT chunk_index,payload FROM artifact_chunks WHERE artifact_id=?",
                               (reference["resource_id"],)).fetchall()
            self.assertTrue(parts)
            db.execute("DELETE FROM artifact_chunks WHERE artifact_id=?", (reference["resource_id"],))
        n.start()
        healthy = self.open()
        claimed = self.next_turn("skip-missing-context")
        self.assertEqual(claimed["session_id"], healthy["resource_id"])
        self.finish(claimed)
        paused = self.status(session)
        self.assertEqual((paused["session"]["lifecycle"], paused["session"]["pause_reason"]),
                         ("Paused", "ResultUnavailable"))
        self.assertEqual(paused["session"]["turn"], 1)
        self.assertIsNone(paused["session"]["current_lease"])
        self.assertEqual(self.next_turn(), {"available": False})
        verification = {"id": tid, "expected_resource_version": tool["task_revision"]}
        unavailable = n.call("Task.VerifyResults", verification)
        self.assertEqual((unavailable["lifecycle"], unavailable["result"]["availability"]),
                         ("Succeeded", "Unavailable"))
        budget = {"id": self.grant["budget_ref"]["resource_id"]}
        self.assertEqual(n.call("Budget.Get", budget, self.agent["token"])["model_calls_used"], 2)
        request = target(paused["reference"])
        for params, token, code in [
            (request, self.provider["token"], "PermissionDenied"),
            (dict(request, expected_resource_version="r1"), self.agent["token"], "Conflict"),
            (request, self.agent["token"], "ResultUnavailable"),
        ]:
            with self.subTest(code=code):
                with self.assertRaises(Failure) as refused:
                    n.call("Session.Resume", params, token)
                self.assertEqual(refused.exception.code, code)
        self.assertEqual(self.status(session), paused)

        n.stop()
        n.start()
        self.assertEqual(self.status(session), paused)
        chunk_bytes = n.call("Artifact.Get", {"reference": reference})["repair_chunk_bytes"]
        for index, payload in parts:
            response = n.call("Artifact.RepairChunk", {
                "reference": reference, "offset": index * chunk_bytes,
                "data_base64": base64.b64encode(payload).decode(),
            })
            self.assertTrue(response["repaired"])
        with self.assertRaises(Failure) as needs_verification:
            n.call("Session.Resume", request, self.agent["token"])
        self.assertEqual(needs_verification.exception.code, "ResultUnavailable")
        restored = n.call("Task.VerifyResults", dict(verification, expected_resource_version=unavailable["task_revision"]))
        self.assertEqual(restored, dict(tool, task_revision=restored["task_revision"]))
        resumed = n.call("Session.Resume", request, self.agent["token"])
        self.assertEqual(resumed["session"]["lifecycle"], "AwaitingModel")
        self.assertEqual(n.call("Budget.Get", budget, self.agent["token"])["model_calls_used"], 2)
        claim = self.next_turn("after-repair")
        self.assertEqual((claim["session_id"], claim["turn"]), (session["resource_id"], 2))
        self.assertEqual(claim["context"]["last_tool"]["local_tool_result"], original)
        self.assertEqual(self.next_turn("after-repair")["lease_id"], claim["lease_id"])
        self.finish(claim)
        self.assertEqual(n.call("Task.Get", {"id": tid}, self.agent["token"]), restored)
        spent = n.call("Budget.Get", budget, self.agent["token"])
        self.assertEqual((spent["model_calls_used"], spent["spent"]), (3, 1))

    def test_long_poll_rechecks_provider_and_does_not_starve_valid_sessions(self):
        peer, empty = self.new_agent(calls=0)
        blocked_budget = self.open(peer, empty)
        blocked_policy = self.open()
        self.n.call("Grant.Revoke", target(self.grant["grant_ref"]))
        valid, grant = self.new_agent()
        with ThreadPoolExecutor(1) as workers:
            waiting = workers.submit(self.next_turn, timeout_ms=5_000)
            time.sleep(.2)
            self.assertFalse(waiting.done())
            sid = self.open(valid, grant)["resource_id"]
            claim = waiting.result(3)
        self.assertEqual(claim["session_id"], sid)
        self.finish(claim)
        self.assertEqual(self.status(blocked_budget, peer)["session"]["pause_reason"], "ModelBudgetExhausted")
        self.assertEqual(self.n.get(blocked_policy["resource_id"])["value"]["pause_reason"], "ModelAuthorizationDenied")
        with ThreadPoolExecutor(1) as workers:
            waiting = workers.submit(self.next_turn, timeout_ms=5_000)
            time.sleep(.2)
            self.assertFalse(waiting.done())
            self.n.call("Grant.Revoke", target(self.provider_grant["grant_ref"]))
            with self.assertRaises(Failure) as denied:
                waiting.result(3)
        self.assertEqual(denied.exception.code, "PermissionDenied")

    def test_model_lease_expiry_fences_the_work_and_respects_session_ceiling(self):
        self.model = self.register_model(lease_seconds=1)
        ref = self.open(max_turns=1)
        original = self.next_turn("original")
        self.n.stop(signal.SIGKILL)
        self.n.start()
        time.sleep(1.05)
        self.assertEqual(self.next_turn(), {"available": False})
        after = self.status(ref)
        self.assertEqual((after["session"]["lifecycle"], after["session"]["pause_reason"]),
                         ("Paused", "ModelTurnLimit"))
        for retry in (lambda: self.next_turn("original"), lambda: self.finish(original)):
            with self.assertRaises(Failure) as denied:
                retry()
            self.assertEqual(denied.exception.code, "Fenced")
        self.n.call("Session.Extend", dict(**target(after["reference"]), max_turns=2), self.agent["token"])
        new_turn = self.next_turn()
        self.assertEqual((new_turn["session_id"], new_turn["turn"]), (ref["resource_id"], 2))
        self.finish(new_turn)

    def test_open_applies_a_model_wide_admission_limit_and_reuses_cancelled_slots(self):
        first = self.open()
        for _ in range(31):
            self.open()
        for _ in range(3):
            agent, grant = self.new_agent()
            for _ in range(32):
                self.open(agent, grant)
        peer, grant = self.new_agent()
        with self.assertRaises(Failure) as full:
            self.open(peer, grant)
        self.assertEqual(full.exception.code, "QueueFull")
        self.n.call("Session.Cancel", target(first), self.agent["token"])
        admitted = self.open(peer, grant)
        self.assertEqual(self.status(admitted, peer)["session"]["lifecycle"], "AwaitingModel")

    def test_disconnected_waiter_does_not_charge_a_later_session(self):
        def request(stream, claim_key):
            payload = json.dumps(dict(schema_version="1.0.0", request_id=key(), token=self.provider["token"],
                method="Model.WaitNext", params=dict(model_ref=self.model, key=claim_key, timeout_ms=5000))).encode()
            stream.connect(str(self.n.socket))
            stream.sendall(struct.pack(">I", len(payload)) + payload)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as lost:
            request(lost, "disconnected")
            time.sleep(.15)
        session = self.open()
        time.sleep(.6)
        self.assertEqual(self.status(session)["session"]["turn"], 0)
        # A caller may finish sending and still receive its lease.
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connected:
            connected.settimeout(3)
            request(connected, "connected")
            connected.shutdown(socket.SHUT_WR)
            result = json.loads(frame(connected)[4:])["result"]
        self.assertEqual(result["session_id"], session["resource_id"])
        self.assertFalse(result["recovered"])
        self.finish(result)

    def test_detached_provider_recovers_exact_decision_before_claiming_new_work(self):
        n = self.n
        for committed in (False, True):
            with self.subTest(committed=committed):
                session = self.open()
                args = self.worker("--once")
                with interrupt_worker_delivery(n, self.token_file, args, committed=committed):
                    self.assertTrue((self.journal / "psios-model-pending.json").exists())
                n.stop(signal.SIGKILL)
                n.start()
                saved_model = self.model_file.read_text()
                self.model_file.write_text(json.dumps(self.register_model()))
                wrong = subprocess.run([n.binaries / "psiosctl", "--socket", n.socket,
                    "--token-file", self.token_file, *args], capture_output=True, text=True, timeout=8)
                self.assertIn("Conflict", wrong.stderr)
                self.model_file.write_text(saved_model)
                replay = n.cli(self.token_file, *args)
                self.assertEqual(replay["session_ref"]["resource_id"], session["resource_id"])
                self.assertEqual(n.wait(replay["task_id"])["lifecycle"], "Succeeded")
                hist = n.call("Session.History", {"id": session["resource_id"]}, self.agent["token"])
                self.assertEqual([t["task_id"] for t in hist["tasks"]], [replay["task_id"]])
                self.assertEqual(len(hist["turns"]), 1)
                self.assertEqual(list(self.journal.iterdir()), [])
                self.n.call("Session.Cancel", target(self.status(session)["reference"]), self.agent["token"])

    def test_worker_finishes_three_turns_without_session_ids(self):
        n = self.n
        command = [n.binaries / "psiosctl", "--socket", n.socket, "--token-file", self.token_file, *self.worker()]
        with subprocess.Popen(command, stdout=subprocess.DEVNULL) as worker:
            try:
                session = self.open()
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and worker.poll() is None:
                    observed = self.status(session)["session"]
                    if observed["lifecycle"] == "Succeeded":
                        break
                    time.sleep(.05)
                else:
                    self.fail("headless provider did not complete session")
                self.assertIn("block devices", observed["summary"])
                hist = n.call("Session.History", {"id": session["resource_id"]}, self.agent["token"])
                self.assertEqual((len(hist["turns"]), len(hist["tasks"])), (3, 2))
            finally:
                if worker.poll() is None:
                    worker.terminate()
                self.assertEqual(worker.wait(6), 0)

    def test_revocation_releases_a_fenced_or_settled_record_without_revealing_the_receipt(self):
        n = self.n
        self.model = self.register_model(max_in_flight=3)
        self.model_file.write_text(json.dumps(self.model))
        for committed, disposition in ((False, "Fenced"), (True, "Settled")):
            with self.subTest(committed=committed):
                agent, grant = self.new_agent()
                session = self.open(agent, grant)
                args = self.worker("--once")
                with interrupt_worker_delivery(n, self.token_file, args, committed=committed):
                    n.call("Grant.Revoke", target(grant["grant_ref"]))
                record = json.loads((self.journal / "psios-model-pending.json").read_text())
                details = dict(model_ref=self.model, key=record["claim_key"],
                               lease_id=record["request"]["lease_id"])
                other, _ = n.agent(["model.serve"])
                with self.assertRaises(Failure) as denied:
                    n.call("Model.ClaimStatus", details, other["token"])
                self.assertEqual(denied.exception.code, "PermissionDenied")
                if not committed:
                    pending = n.call("Model.ClaimStatus", details, self.provider["token"])
                    self.assertEqual(pending, {"status": "Pending"})
                    current = n.get(session["resource_id"])
                    n.call("Session.Cancel", target(current["reference"]), agent["token"])
                self.assertEqual(n.cli(self.token_file, *args), {"status": disposition})
                self.assertEqual(list(self.journal.iterdir()), [])
        # A revoked session cannot strand the worker's shared decision slot.
        next_session = self.open()
        claim = self.next_turn()
        self.assertEqual(claim["session_id"], next_session["resource_id"])
        self.finish(claim)

    def test_durable_schedule_admits_one_session_then_provider_finishes_its_tools(self):
        n = self.n
        grant = self.grant_for_start("schedule.manage", "session.run", *self.tools)
        owner = self.agent["token"]
        at = datetime.now(timezone.utc) + timedelta(seconds=2)
        request = dict(key="scheduled-local-survey", tool="agent.session.start", input=self.blueprint(),
                       delegation_id=grant["delegation_ref"]["resource_id"],
                       run_at=at.isoformat(), deadline_at=(at + timedelta(seconds=30)).isoformat())
        schedule = n.call("Schedule.Create", request, owner)
        n.stop(signal.SIGKILL)
        time.sleep(2.1)
        n.start()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            observed = n.call("Schedule.Get", {"id": schedule["reference"]["resource_id"]}, owner)["state"]
            if observed["lifecycle"] != "Pending":
                break
            time.sleep(.05)
        self.assertEqual(observed["lifecycle"], "Triggered", observed)
        admitted = n.wait(observed["task_receipt"]["task_id"])
        self.assertEqual(admitted["lifecycle"], "Succeeded")
        output = n.call("Artifact.Read", {"reference": admitted["result"]["artifact_refs"][0]}, owner)
        session = output["session_ref"]
        self.assertEqual(self.status(session)["session"]["origin_task_ref"]["resource_id"], admitted["task_id"])
        n.stop(signal.SIGKILL)
        n.start()
        repeated = n.call("Schedule.Create", request, owner)
        self.assertEqual(repeated["state"]["task_receipt"], observed["task_receipt"])
        for turn in range(3):
            result = n.cli(self.token_file, *self.worker("--once"))
            self.assertEqual(result["session_ref"]["resource_id"], session["resource_id"])
            if turn < 2:
                self.assertEqual(n.wait(result["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(self.status(session)["session"]["lifecycle"], "Succeeded")
        usage = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, owner)
        self.assertEqual((usage["spent"], usage["model_calls_used"]), (3, 3))
        self.assertEqual(self.next_turn(), {"available": False})

    def test_session_admission_respects_both_targets_and_rolls_back_an_ungranted_session(self):
        n = self.n
        token = self.agent["token"]
        scoped = self.grant_for_start("session.run", *self.tools, tool_targets=[self.model["resource_id"]])
        prepared = n.prepare("agent.session.start", self.blueprint(), token=token,
                             delegation=scoped["delegation_ref"]["resource_id"])
        with self.assertRaises(Failure) as scope:
            n.call("Task.Submit", prepared, token)
        self.assertEqual(scope.exception.code, "PermissionDenied")
        denied = self.grant_for_start()
        params = n.prepare("agent.session.start", self.blueprint(), token=token,
                           delegation=denied["delegation_ref"]["resource_id"])
        task = n.wait(n.call("Task.Submit", params, token)["task_id"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertEqual(task["conditions"][0]["type"], "PermissionDenied")
        budget = n.call("Budget.Get", {"id": denied["budget_ref"]["resource_id"]}, token)
        self.assertEqual((budget["reserved"], budget["spent"], budget["model_calls_used"]), (0, 0, 0))
        self.assertEqual(self.next_turn(), {"available": False})
        with self.assertRaises(Failure) as recursion:
            n.call("Skill.Publish", dict(name="recursive", version="1", tools=["agent.session.start"],
                source="local-test", license="0BSD", instructions="Schedule more sessions."))
        self.assertEqual(recursion.exception.code, "UnsupportedCapability")
