"""Process-level coverage of persisted sessions across distinct agent, provider and owner credentials."""
import signal
import time
import unittest
from concurrent.futures import ThreadPoolExecutor

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
