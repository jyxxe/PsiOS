"""Process-level coverage of persisted sessions across distinct agent, provider and owner credentials."""
import json
import os
import signal
import subprocess
import sqlite3
import time
import unittest
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait

from model_delivery import interrupt_delivery
from support import Failure, Node, key, target


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)
        self.agent = self.n.call("Agent.Register", {"name": "observer"})
        self.provider = self.n.call("Agent.Register", {"name": "local-model"})
        self.n.call("Grant.Issue", dict(principal_id=self.provider["principal_id"], actions=["model.serve"], units=1, ttl_seconds=90))
        self.tools = ["node.inventory", "node.devices"]
        self.grant = self.n.call("Grant.Issue", dict(principal_id=self.agent["principal_id"], units=8, model_calls=5,
            actions=self.tools + ["session.run", "task.read", "task.submit", "resource.read", "mail.receive"], ttl_seconds=90))
        self.skill = self.n.call("Skill.Publish", dict(name="inventory", version="1", tools=self.tools, source="local-test",
            license="0BSD", instructions="Read node inventory and count the devices."))
        self.model = self.register_model(15)
        self.token = self.agent["token"]
        self.provider_token = self.provider["token"]

    def register_model(self, lease):
        return self.n.call("Model.Register", dict(name="inventory-provider", revision=str(lease),
            provider_principal_id=self.provider["principal_id"], lease_seconds=lease,
            max_context_bytes=131072, max_output_bytes=8192))

    def options(self, **kwargs):
        return dict(dict(goal="Report storage devices", key=key(), model_ref=self.model, skill_ref=self.skill,
            context_policy="LocalToolResults", max_turns=4, delegation_id=self.grant["delegation_ref"]["resource_id"]), **kwargs)

    def open(self, **kwargs):
        return self.n.call("Session.Open", self.options(**kwargs), self.token)["reference"]["resource_id"]

    def claim(self, s, k):
        return self.n.call("Model.Claim", {"session_id": s, "key": k}, self.provider_token)

    def respond(self, s, lease, decision):
        return self.n.call("Model.Respond", dict(session_id=s, lease_id=lease, decision=decision), self.provider_token)

    def denied(self, code, func, *args):
        with self.assertRaises(Failure) as error:
            func(*args)
        self.assertEqual(error.exception.code, code)

    def test_two_principal_native_provider_persistent_tool_chain(self):
        n = self.n
        sid = self.open()
        f = n.base / "model.token"
        f.write_text(self.provider_token)
        f.chmod(0o600)
        self.denied("PermissionDenied", n.call, "Session.Get", {"id": sid}, self.provider_token)
        self.denied("PermissionDenied", n.call, "Model.Claim", {"session_id": sid, "key": "x"}, self.token)
        for i, tool in enumerate(self.tools):
            step = n.cli(f, "model-step", sid, "--key", f"turn-{i}")
            t = n.wait(step["task_id"])
            self.assertEqual(t["lifecycle"], "Succeeded")
            run = n.call("Run.Get", {"id": t["current_run_id"]}, self.token)
            snapshot = n.call("Resource.Resolve", {"reference": run["execution_snapshot_ref"]}, self.token)["value"]
            self.assertEqual(snapshot["model_ref"], self.model)
            self.assertEqual(snapshot["skill_ref"], self.skill)
            self.assertEqual(snapshot["session_ref"]["resource_id"], sid)
            self.assertEqual(snapshot["tool_refs"][0]["resource_id"], "tool-" + tool)
            n.stop(signal.SIGKILL)
            n.start()
            self.assertEqual(n.cli(f, "model-step", sid, "--key", f"turn-{i}"), step)
        final = n.cli(f, "model-step", sid, "--key", "finish")
        self.assertEqual(final["lifecycle"], "Succeeded")
        s = n.call("Session.Get", {"id": sid}, self.token)
        self.assertIn("block devices", s["session"]["summary"])
        self.assertEqual(len(n.call("Session.History", {"id": sid}, self.token)["turns"]), 3)
        budget = n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]}, self.token)
        self.assertEqual((budget["model_calls_used"], budget["spent"]), (3, 2))

    def test_witness_reserve_blocks_new_leases_but_allows_model_settlement(self):
        n = self.n
        first, waiting = self.open(), self.open()
        lease = self.claim(first, "in-flight")
        cfg = json.loads(n.config.read_text())
        st = os.statvfs(n.witness)
        n.stop()
        n.config.write_text(json.dumps(dict(cfg, authority_reserve={"witness_bytes": st.f_blocks * st.f_frsize + 1})))
        n.start()
        self.assertTrue(n.call("Node.Get", {})["authority_storage"]["witness"]["below_reserve"])
        self.denied("QuotaExceeded", self.claim, waiting, "pending")
        self.assertEqual(self.claim(first, "in-flight")["lease_id"], lease["lease_id"])
        result = self.respond(first, lease["lease_id"], dict(action="finish", summary="Retained"))
        self.assertEqual(result["lifecycle"], "Succeeded")
        self.assertEqual(self.claim(first, "in-flight")["completed"], result)
        budget = n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]}, self.token)
        self.assertEqual(budget["model_calls_used"], 1)

    def test_agent_keeps_latest_local_result_while_a_paused_session_can_resume(self):
        n = self.n
        sid = self.open(max_turns=1)
        lease = self.claim(sid, "observe")
        result = self.respond(sid, lease["lease_id"], dict(action="tool", tool="node.inventory", input={}))
        task = n.wait(result["task_id"])
        reference, = task["result"]["artifact_refs"]
        body = n.call("Artifact.Read", {"reference": reference}, self.token)
        request = {"id": task["task_id"], "expected_resource_version": task["task_revision"], "reason": "finished"}
        paused = n.call("Session.Get", {"id": sid}, self.token)
        self.assertEqual(paused["session"]["pause_reason"], "ModelTurnLimit")
        self.denied("Conflict", n.call, "Task.EvictResults", request)
        n.stop(signal.SIGKILL)
        n.start()
        self.denied("Conflict", n.call, "Task.EvictResults", request)
        n.call("Session.Extend", {**target(paused["reference"]), "max_turns": 2}, self.token)
        resumed = self.claim(sid, "consume")
        self.assertEqual(resumed["context"]["last_tool"]["local_tool_result"], body)
        done = self.respond(sid, resumed["lease_id"], {"action": "finish", "summary": "Inventory saved"})
        self.assertEqual(done["lifecycle"], "Succeeded")
        n.call("Task.EvictResults", request)
        described = n.call("Artifact.Get", {"reference": reference}, self.token)
        self.assertEqual(described["availability"], "Evicted")
        self.denied("ResultEvicted", n.call, "Artifact.Read", {"reference": reference}, self.token)

    def test_durable_provider_delivery_survives_both_sides_of_the_response(self):
        n = self.n
        pending = n.base / "provider"
        pending.mkdir(mode=0o700)
        record = pending / "psios-model-pending.json"
        token = n.base / "provider.token"
        token.write_text(self.provider_token)
        token.chmod(0o600)
        sid = self.open()

        def invoke(session, key, credential=token):
            return subprocess.run([n.binaries / "psiosctl", "--socket", n.socket, "--token-file", credential,
                                   "model-step", session, "--key", key, "--decision-dir", pending],
                                  capture_output=True, text=True, timeout=10)

        for i, committed in enumerate((False, True)):
            key = f"turn-{i}"
            with interrupt_delivery(n, token, sid, key, pending, committed=committed):
                self.assertIn("Conflict", invoke(sid, key).stderr)
                self.assertEqual(record.stat().st_mode & 0o777, 0o600)
            stored = json.loads(record.read_text())
            self.assertEqual(stored["request"]["decision"], dict(action="tool", tool=self.tools[i], input={}))
            self.assertNotIn(self.provider_token, record.read_text())
            self.assertNotIn("context", stored)
            self.assertIn("Conflict", invoke(sid, "other-key").stderr)
            self.assertIn("Conflict", invoke(sid, key, n.witness / "owner.token").stderr)
            if i == 0:
                other = Node("target/debug")
                try:
                    wrong_node = subprocess.run([other.binaries / "psiosctl", "--socket", other.socket,
                        "--token-file", other.witness / "owner.token", "model-step", sid, "--key", key,
                        "--decision-dir", pending], capture_output=True, text=True)
                    self.assertIn("Conflict", wrong_node.stderr)
                finally:
                    other.close()
            self.assertTrue(record.exists())
            history = n.call("Session.History", {"id": sid}, self.token)
            self.assertEqual(len(history["tasks"]), i + int(committed))
            accepted = history["tasks"][-1]["task_id"] if committed else None
            if accepted:
                # Keep a worker crash during a read out of this delivery test.
                self.assertEqual(n.wait(accepted)["lifecycle"], "Succeeded")
            n.stop(signal.SIGKILL)
            n.start()
            result = invoke(sid, key)
            self.assertEqual(result.returncode, 0, result.stderr)
            task = json.loads(result.stdout)["task_id"]
            if accepted:
                self.assertEqual(task, accepted)
            self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
            self.assertFalse(record.exists())
            replay = invoke(sid, key)
            self.assertEqual(json.loads(replay.stdout)["task_id"], task)
        history = n.call("Session.History", {"id": sid}, self.token)
        self.assertEqual((len(history["turns"]), len(history["tasks"])), (2, 2))
        budget = n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]}, self.token)
        self.assertEqual((budget["model_calls_used"], budget["spent"]), (2, 2))

    def test_durable_provider_respects_fencing_and_private_storage(self):
        n = self.n
        pending = n.base / "provider"
        pending.mkdir(mode=0o700)
        token = n.base / "model.token"
        token.write_text(self.provider_token)
        token.chmod(0o600)
        sid = self.open()
        args = [n.binaries / "psiosctl", "--socket", n.socket, "--token-file", token,
                "model-step", sid, "--key", "turn", "--decision-dir", pending]
        pending.chmod(0o755)
        refused = subprocess.run(args, capture_output=True, text=True)
        self.assertIn("PermissionDenied", refused.stderr)
        self.assertFalse(n.call("Session.History", {"id": sid}, self.token)["turns"])
        pending.chmod(0o700)
        with interrupt_delivery(n, token, sid, "turn", pending, committed=False):
            pass
        n.call("Session.Cancel", target(n.call("Session.Get", {"id": sid}, self.token)["reference"]), self.token)
        refused = subprocess.run(args, capture_output=True, text=True)
        self.assertIn("Fenced", refused.stderr)
        self.assertEqual(list(pending.iterdir()), [])
        self.assertFalse(n.call("Session.History", {"id": sid}, self.token)["tasks"])

    def test_missing_or_unreadable_decision_never_restarts_inference(self):
        n = self.n
        journal = n.base / "provider"
        journal.mkdir(mode=0o700)
        token = n.base / "model.token"
        token.write_text(self.provider_token)
        token.chmod(0o600)
        sid = self.open()
        self.claim(sid, "started")
        args = [n.binaries / "psiosctl", "--socket", n.socket, "--token-file", token, "model-step",
                sid, "--key", "started", "--decision-dir", journal]
        partial = journal / ".psios-model-pending.tmp"
        partial.write_bytes(b'{"incomplete"')
        self.assertIn("OutcomeUnknown", subprocess.run(args, capture_output=True, text=True).stderr)
        self.assertFalse(partial.exists())
        pending = journal / "psios-model-pending.json"
        pending.write_bytes(b'{"incomplete"')
        pending.chmod(0o600)
        self.assertIn("RecoveryRequired", subprocess.run(args, capture_output=True, text=True).stderr)
        self.assertEqual(pending.read_bytes(), b'{"incomplete"')
        history = n.call("Session.History", {"id": sid}, self.token)
        self.assertEqual((len(history["turns"]), history["tasks"]), (1, []))

    def test_reported_tokens_bind_once_to_the_accepted_decision(self):
        n = self.n
        sid = self.open()
        claim = self.claim(sid, "metered")
        self.assertEqual(claim["usage_format"], "tokens.v1")
        request = dict(session_id=sid, lease_id=claim["lease_id"], decision=dict(action="finish", summary="done"),
                       usage=dict(input_tokens=17, output_tokens=4))
        def respond(p):
            return n.call("Model.Respond", p, self.provider_token)
        for bad in ({"input_tokens": -1, "output_tokens": 2}, {"output_tokens": 1},
                    {"input_tokens": 2**32, "output_tokens": 1}, {"input_tokens": 0, "output_tokens": 1, "cost": 2}):
            self.denied("InvalidArgument", respond, dict(request, usage=bad))
        with ThreadPoolExecutor(2) as workers:
            a, b = workers.map(respond, [request, request])
        self.assertEqual(a, b)
        ref = a["usage_ref"]
        observation = n.call("Resource.Resolve", {"reference": ref}, self.token)["value"]
        self.assertEqual(observation["reported_tokens"], request["usage"])
        self.assertEqual(observation["measurement_source"], "ProviderReported")
        self.assertEqual(observation["provider_principal_id"], self.provider["principal_id"])
        self.assertEqual(observation["budget_ref"], self.grant["budget_ref"])
        self.assertEqual(observation["call_id"], claim["lease_id"])
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(respond(request), a)
        self.denied("IdempotencyConflict", respond, dict(request, usage=dict(input_tokens=17, output_tokens=5)))
        self.denied("IdempotencyConflict", respond, {k:v for k,v in request.items() if k != "usage"})
        self.assertEqual(n.get(ref["resource_id"])["reference"], ref)

        sid = self.open()
        lease = self.claim(sid, "unreported")["lease_id"]
        bare = dict(session_id=sid, lease_id=lease, decision=dict(action="finish", summary="done"))
        self.assertNotIn("usage_ref", respond(bare))
        self.denied("IdempotencyConflict", respond, dict(bare, usage=dict(input_tokens=0, output_tokens=0)))
        budget = n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]}, self.token)
        self.assertEqual((budget["model_calls_used"], budget["spent"]), (2, 0))

    def test_competing_leases_duplicate_response_and_restricted_context(self):
        n = self.n
        opts = self.options(context_policy="ReferencesOnly", max_turns=1)
        current = n.call("Session.Open", opts, self.token)
        self.assertEqual(n.call("Session.Open", opts, self.token), current)
        self.denied("IdempotencyConflict", n.call, "Session.Open", dict(opts, goal="other"), self.token)
        sid = current["reference"]["resource_id"]

        def race(k):
            try:
                return self.claim(sid, k)
            except Failure as e:
                return e.code

        with ThreadPoolExecutor(4) as pool:
            replies = list(pool.map(race, [str(i) for i in range(4)]))
        self.assertEqual(replies.count("Conflict"), 3)
        lease = next(x["lease_id"] for x in replies if isinstance(x, dict))
        decision = dict(action="tool", tool="node.inventory", input={})
        self.denied("PermissionDenied", self.respond, sid, lease, dict(decision, tool="volume.create"))
        with ThreadPoolExecutor(2) as pool:
            a, b = list(pool.map(lambda _: self.respond(sid, lease, decision), range(2)))
        self.assertEqual(a, b)
        self.denied("IdempotencyConflict", self.respond, sid, lease, dict(action="finish", summary="changed"))
        self.assertEqual(n.wait(a["task_id"])["lifecycle"], "Succeeded")
        session = n.call("Session.Get", {"id": sid}, self.token)
        self.assertEqual(session["session"]["pause_reason"], "ModelTurnLimit")
        n.call("Session.Extend", dict(**target(session["reference"]), max_turns=2), self.token)
        next_turn = self.claim(sid, "new")
        self.assertNotIn("local_tool_result", next_turn["context"]["last_tool"])
        self.assertEqual(next_turn["context"]["last_tool"]["result"]["availability"], "Available")

    def test_expired_provider_fencing_and_shared_root_allowance(self):
        sid = self.open(model_ref=self.register_model(1), max_turns=2)
        a = self.claim(sid, "before")
        n = self.n
        n.stop(signal.SIGKILL)
        n.start()
        time.sleep(1.05)
        b = self.claim(sid, "after")
        self.assertNotEqual(a["lease_id"], b["lease_id"])
        self.denied("Fenced", self.respond, sid, a["lease_id"], {"action": "finish", "summary": "late"})
        self.respond(sid, b["lease_id"], {"action": "fail", "reason": "offline provider"})
        for _ in range(3):
            s = self.open()
            self.claim(s, "one")
        self.denied("QuotaExceeded", self.claim, self.open(), "over")
        self.assertEqual(n.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]})["model_calls_used"], 5)

    def test_failed_session_task_retry_keeps_parent_and_deduplicates_receipt(self):
        n = self.n
        grant = n.call("Grant.Issue", dict(principal_id=self.agent["principal_id"], units=6, model_calls=2,
            actions=["session.run", "task.submit", "task.read", "volume.create", "volume.read"], ttl_seconds=90))
        did = grant["delegation_ref"]["resource_id"]
        make = n.prepare("volume.create", {"name": "owned"}, key(), self.token, did)
        task = n.wait(n.call("Task.Submit", make, self.token)["task_id"])
        vol = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, self.token)
        vol = n.perform("volume.write", dict(**target(vol), name="record", content="durable"))["volume_ref"]
        skill = n.call("Skill.Publish", dict(name="reader", version="1", source="local", license="0BSD",
            instructions="Read the owned record", tools=["volume.read"]))
        sid = self.open(skill_ref=skill, delegation_id=did)
        data = n.state / "volumes" / vol["resource_id"] / "data"
        before = data.with_name("temporarily-unmounted")
        data.rename(before)
        c = self.claim(sid, "read")
        receipt = self.respond(sid, c["lease_id"], dict(action="tool", tool="volume.read", input=dict(**target(vol), name="record")))
        failed = n.wait(receipt["task_id"])
        self.assertEqual(failed["lifecycle"], "Failed")
        before.rename(data)
        self.denied("PreconditionFailed", n.call, "Task.Retry", dict(id=receipt["task_id"], key="no-parent",
            expected_resource_version=failed["task_revision"]), self.token)
        req = dict(**target(n.call("Session.Get", {"id": sid}, self.token)["reference"]), key="retry-read", task_id=receipt["task_id"])
        retry = n.call("Session.RetryTask", req, self.token)
        done = n.wait(retry["task_id"])
        self.assertEqual(done["retry_of_task_id"], receipt["task_id"])
        self.assertEqual(n.call("Artifact.Read", {"reference": done["result"]["artifact_refs"][0]}, self.token)["content"], "durable")
        again = n.call("Session.RetryTask", req, self.token)
        self.assertTrue(again["deduplicated"])
        self.assertEqual(again["task_id"], retry["task_id"])
        history = n.call("Session.History", {"id": sid}, self.token)
        self.assertEqual([x["attempt"] for x in history["tasks"]], [1, 2])
        self.assertEqual(len(history["turns"]), 1)

    def test_provider_concurrency_and_inflight_revocation_accounting(self):
        model = self.n.call("Model.Register", dict(name="one-at-a-time", revision="a", provider_principal_id=self.provider["principal_id"],
            lease_seconds=30, max_context_bytes=8192, max_output_bytes=1024, max_in_flight=1))
        a, b = self.open(model_ref=model), self.open(model_ref=model)
        c = self.claim(a, "first")
        self.denied("QueueFull", self.claim, b, "busy")
        self.respond(a, c["lease_id"], dict(action="finish", summary="done"))
        self.claim(b, "busy")
        residual = self.n.call("Grant.Revoke", target(self.grant["grant_ref"]))
        self.assertEqual(residual["status"], "Partial")
        self.denied("PermissionDenied", self.claim, b, "busy")

    def test_wall_clock_projection_does_not_release_live_provider_work(self):
        model = self.n.call("Model.Register", dict(name="one-slot", revision="elapsed",
            provider_principal_id=self.provider["principal_id"], lease_seconds=90, max_context_bytes=8192,
            max_output_bytes=1024, max_in_flight=1))
        a, b = self.open(model_ref=model), self.open(model_ref=model)
        lease = self.claim(a, "long")
        self.n.stop()
        # Set just the displayed UTC deadline in this private database to the past.
        with sqlite3.connect(self.n.state / "state.db") as db:
            db.execute("UPDATE session_turns SET expires=? WHERE lease=?", ("2000-01-01T00:00:00Z", lease["lease_id"]))
        self.n.start()
        self.assertEqual(self.claim(a, "long")["lease_id"], lease["lease_id"])
        self.denied("QueueFull", self.claim, b, "busy")
        current = self.n.call("Session.Get", {"id": a}, self.token)
        self.n.call("Session.Cancel", target(current["reference"]), self.token)
        self.denied("QueueFull", self.claim, b, "busy")
        self.assertEqual(self.n.call("Grant.Revoke", target(self.grant["grant_ref"]))["status"], "Partial")

    def test_revocation_retirement_and_cancel_close_live_leases(self):
        n = self.n
        s = self.open()
        lease = self.claim(s, "a")["lease_id"]
        n.call("Session.Cancel", target(n.call("Session.Get", {"id": s}, self.token)["reference"]), self.token)
        self.denied("Fenced", self.respond, s, lease, dict(action="tool", tool=self.tools[0], input={}))
        s = self.open()
        lease = self.claim(s, "b")["lease_id"]
        n.call("Skill.Retire", target(self.skill))
        n.stop()
        n.start()
        self.denied("PreconditionFailed", self.respond, s, lease, dict(action="finish", summary="old code"))
        n.call("Grant.Revoke", target(self.grant["grant_ref"]))
        self.denied("PermissionDenied", self.claim, s, "new")
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])

    def test_inbox_wait_delivery_replay_and_closed_drain(self):
        n = self.n
        sender, _ = n.agent(["mail.send", "mail.receive"])
        mailbox = n.call("Mailbox.Bind", dict(name="jobs", sender_principal_id=sender["principal_id"]), self.token)
        box = mailbox["resource_id"]
        params = dict(id=box, timeout_ms=5000, limit=1)
        self.denied("PermissionDenied", n.call, "Mailbox.Wait", params, sender["token"])
        for invalid in [dict(id=box), dict(params, timeout_ms=0), dict(params, timeout_ms=10001),
                        dict(params, timeout_ms=True), dict(params, offset=1), dict(params, limit=0)]:
            self.denied("InvalidArgument", n.call, "Mailbox.Wait", invalid, self.token)
        credential = n.base / "reader.token"
        credential.write_text(self.token)
        credential.chmod(0o600)
        start = time.monotonic()
        empty = n.cli(credential, "inbox", box, "--wait-ms", "150", "--limit", "1")
        self.assertGreaterEqual(time.monotonic() - start, .12)
        self.assertEqual((empty["items"], empty["lifecycle"]), ([], "Open"))
        with ThreadPoolExecutor(1) as pool:
            pending = pool.submit(n.call, "Mailbox.Wait", params, self.token)
            time.sleep(.1)
            self.assertFalse(pending.done())
            event = dict(mailbox_id=box, topic="WorkAvailable", key="first")
            first = n.call("Signal.Send", event, sender["token"])
            page = pending.result(2)
            self.assertEqual([item["reference"] for item in page["items"]], [first])
        second = n.call("Signal.Send", dict(event, key="second"), sender["token"])
        n.stop(signal.SIGKILL)
        n.start()
        replay = n.cli(credential, "inbox", box, "--limit", "1")
        self.assertEqual((replay["items"], replay["acked"]), (page["items"], 0))
        n.call("Mailbox.Close", target(replay["mailbox_ref"]), self.token)
        self.assertEqual(n.call("Mailbox.Wait", params, self.token)["items"], page["items"])
        n.call("Mailbox.Ack", dict(id=box, previous=0, through=1), self.token)
        page = n.call("Mailbox.Wait", params, self.token)
        self.assertEqual([item["reference"] for item in page["items"]], [second])
        n.call("Mailbox.Ack", dict(id=box, previous=1, through=2), self.token)
        with ThreadPoolExecutor(1) as pool:
            drained = pool.submit(n.call, "Mailbox.Wait", params, self.token).result(2)
        self.assertEqual((drained["items"], drained["lifecycle"], drained["acked"]), ([], "Closed", 2))

    def test_mailbox_and_event_wait_share_slots_and_wake_on_close_or_revoke(self):
        n = self.n
        box = n.call("Mailbox.Bind", dict(name="queue", sender_principal_id=self.provider["principal_id"]), self.token)
        params = dict(id=box["resource_id"], timeout_ms=10000)
        cursor = n.call("Resource.List", dict(kind="Node"))["watch_start_cursor"]
        with ThreadPoolExecutor(9) as pool:
            inbox = [pool.submit(n.call, "Mailbox.Wait", params, self.token) for _ in range(3)]
            done, sleeping_inbox = wait(inbox, timeout=3, return_when=FIRST_COMPLETED)
            self.assertEqual(len(done), 1)
            self.assertEqual(len(sleeping_inbox), 2)
            self.denied("QueueFull", done.pop().result)
            events = [pool.submit(n.call, "Event.Wait", dict(cursor=cursor, timeout_ms=10000)) for _ in range(7)]
            done, sleeping_events = wait(events, timeout=3, return_when=FIRST_COMPLETED)
            self.assertEqual(len(done), 1)
            self.assertEqual(len(sleeping_events), 6)
            self.denied("QueueFull", done.pop().result)
            n.call("Mailbox.Close", target(box), self.token)
            node = n.call("Node.Get", {})
            changed = n.call("Node.Configure", dict(scheduling="Cordoned", expected_resource_version=node["reference"]["version"]))
            for future in sleeping_inbox | sleeping_events:
                result = future.result(2)
                if future in sleeping_inbox:
                    self.assertEqual(result["lifecycle"], "Closed")
                else:
                    self.assertEqual(result["events"][0]["resource_ref"], changed)
            box = n.call("Mailbox.Bind", dict(name="new", sender_principal_id=self.provider["principal_id"]), self.token)
            listener = pool.submit(n.call, "Mailbox.Wait", dict(params, id=box["resource_id"]), self.token)
            time.sleep(.1)
            n.call("Grant.Revoke", target(self.grant["grant_ref"]))
            self.denied("PermissionDenied", listener.result, 2)

    def test_mailbox_to_session_exactly_once_and_no_implicit_resource_sharing(self):
        n = self.n
        sender, _ = n.agent(["mail.send", "resource.read", "task.read"])
        mailbox = n.call("Mailbox.Bind", dict(name="review", sender_principal_id=sender["principal_id"]), self.token)
        event = dict(mailbox_id=mailbox["resource_id"], key="job-1", topic="ReviewRequested")
        foreign = n.perform("volume.create", {"name": "private"})
        self.denied("PermissionDenied", n.call, "Signal.Send", dict(event, resource_ref=foreign), sender["token"])
        sent = n.call("Signal.Send", event, sender["token"])
        self.assertEqual(sent, n.call("Signal.Send", event, sender["token"]))
        self.denied("IdempotencyConflict", n.call, "Signal.Send", dict(event, topic="WorkAvailable"), sender["token"])
        self.denied("Conflict", n.call, "Mailbox.Ack", dict(id=mailbox["resource_id"], previous=0, through=1), self.token)
        n.stop(signal.SIGKILL)
        n.start()
        delivery = n.call("Mailbox.Read", {"id": mailbox["resource_id"]}, self.token)["items"]
        self.assertEqual([d["reference"] for d in delivery], [sent])
        sid = self.open(trigger_signal_ref=sent)
        self.denied("IdempotencyConflict", n.call, "Session.Open", self.options(trigger_signal_ref=sent), self.token)
        step = self.claim(sid, "signal")
        result = self.respond(sid, step["lease_id"], dict(action="tool", tool="node.inventory", input={}))
        self.assertEqual(n.wait(result["task_id"])["lifecycle"], "Succeeded")
        ack = n.call("Mailbox.Ack", dict(id=mailbox["resource_id"], previous=0, through=1), self.token)
        self.assertEqual(n.call("Mailbox.Read", {"id": mailbox["resource_id"]}, self.token)["items"], [])
        n.call("Mailbox.Close", target(ack), self.token)
        self.denied("PermissionDenied", n.call, "Signal.Send", dict(event, key="later"), sender["token"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
