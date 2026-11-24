"""Schedule real Tasks through deadlines, restart, approval and competing admission."""
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
import json
import os
import signal
import time
import unittest

from support import Failure, Node, key, target


def timestamp(seconds):
    return (datetime.now(timezone.utc) + timedelta(seconds=seconds)).isoformat(timespec="milliseconds")


def after(instant, grace=0.1):
    delay = (datetime.fromisoformat(instant) - datetime.now(timezone.utc)).total_seconds() + grace
    if delay > 0:
        time.sleep(delay)


class ScheduleTests(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)

    def grant(self, tool="system.status", resource="profile-local", principal="local-owner"):
        return self.node.call("Grant.Issue", dict(principal_id=principal, tool_targets=[resource],
                              actions=["schedule.manage", "task.submit", "task.read", tool],
                              units=1, ttl_seconds=60))

    def request(self, grant, tool="system.status", input=None, run=1.5, deadline=15):
        return dict(key=key(), tool=tool, input={} if input is None else input,
                    delegation_id=grant["delegation_ref"]["resource_id"], run_at=timestamp(run),
                    deadline_at=timestamp(deadline))

    def state(self, schedule, token=None):
        return self.node.call("Schedule.Get", {"id": schedule["reference"]["resource_id"]}, token)

    def wait(self, schedule, lifecycle):
        deadline = time.monotonic() + 10
        while True:
            result = self.state(schedule)
            if result["state"]["lifecycle"] != "Pending" or time.monotonic() >= deadline:
                self.assertEqual(result["state"]["lifecycle"], lifecycle, result)
                return result
            time.sleep(0.05)

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as reason:
            self.node.call(method, params, token)
        self.assertEqual(reason.exception.code, code, reason.exception)

    def budget(self, grant):
        result = self.node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        return result["reserved"], result["spent"]

    def test_restart_recovers_one_trigger_and_preserves_real_approval(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "scheduled-data"})
        grant = self.grant("volume.write", volume["resource_id"])
        request = self.request(grant, "volume.write", dict(**target(volume), name="timed", content="durable"))
        with ThreadPoolExecutor(max_workers=4) as callers:
            created = list(callers.map(lambda _: n.call("Schedule.Create", request), range(4)))
        initial = created[0]
        self.assertEqual(created, [initial] * 4)
        self.assertEqual(initial["state"]["lifecycle"], "Pending")
        self.assertEqual(initial["state"]["timezone"], "UTC")
        self.assertEqual(self.budget(grant), (0, 0))
        prepared = n.get(initial["reference"]["resource_id"])["resource"]["spec"]["request"]
        self.rejected("PermissionDenied", "Task.Submit", prepared)
        self.rejected("IdempotencyConflict", "Schedule.Create", dict(request, input=dict(request["input"], content="changed")))
        n.stop(signal.SIGKILL)
        after(request["run_at"])
        n.start()
        triggered = self.wait(initial, "Triggered")
        receipt = triggered["state"]["task_receipt"]
        task = receipt["task_id"]
        self.assertEqual(receipt["origin_ref"]["origin_type"], "Schedule")
        self.assertEqual(receipt["origin_ref"]["origin_id"], initial["reference"]["resource_id"])
        self.assertEqual(receipt["idempotency_key"], initial["state"]["trigger_id"])
        self.assertEqual(n.wait(task)["lifecycle"], "WaitingApproval")
        self.assertEqual(self.budget(grant), (1, 0))
        data = n.state / "volumes" / volume["resource_id"] / "data" / "timed"
        self.assertFalse(data.exists())
        n.stop(signal.SIGKILL)
        n.start()
        offset = timezone(timedelta(hours=5, minutes=30))
        repeated = dict(request, run_at=datetime.fromisoformat(request["run_at"]).astimezone(offset).isoformat())
        self.assertEqual(n.call("Schedule.Create", repeated), triggered)
        self.rejected("Conflict", "Schedule.Cancel", target(triggered["reference"]))
        n.approve(task)
        self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
        self.assertEqual(data.read_text(), "durable")
        self.assertEqual(self.budget(grant), (0, 1))
        self.assertEqual(len(n.call("Resource.List", {"kind": "Task"})["items"]), 2)

    def test_scope_revocation_conditional_cancel_and_missed_deadline(self):
        n = self.node
        grant = self.grant()
        request = self.request(grant, run=2, deadline=5)
        self.rejected("PermissionDenied", "Schedule.Create", dict(request, delegation_id="owner-delegation"))
        self.rejected("PermissionDenied", "Schedule.Create", dict(request, tool="node.inventory"))
        self.rejected("InvalidArgument", "Schedule.Create", dict(request, deadline_at=timestamp(90)))
        cancelled = n.call("Schedule.Create", request)
        self.rejected("Conflict", "Schedule.Cancel", dict(target(cancelled["reference"]), expected_resource_version="r99"))
        current = n.call("Schedule.Cancel", target(cancelled["reference"]))
        self.assertEqual(current["state"]["lifecycle"], "Cancelled")
        self.assertEqual(n.call("Schedule.Create", request), current)
        revoked = n.call("Schedule.Create", self.request(grant, run=1, deadline=10))
        bootstrap = n.get(n.get("owner-delegation")["value"]["grant_id"])["reference"]
        self.rejected("PermissionDenied", "Grant.Revoke", target(bootstrap))
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        rejection = self.wait(revoked, "Rejected")
        self.assertEqual(rejection["state"]["reason"], "PermissionDenied")
        self.assertEqual(self.budget(grant), (0, 0))
        self.assertIsNone(rejection["state"]["task_receipt"])
        grant = self.grant()
        request = self.request(grant, run=1, deadline=1.6)
        missed = n.call("Schedule.Create", request)
        n.stop(signal.SIGKILL)
        after(request["deadline_at"])
        n.start()
        self.assertEqual(self.wait(missed, "Expired")["state"]["reason"], "DeadlineExceeded")
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])
        self.assertEqual(self.budget(grant), (0, 0))

    def test_queue_pressure_and_cordon_hold_the_trigger_without_spending(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "admission-gate"})
        filler = n.submit("volume.write", dict(**target(volume), name="held", content="hold"))
        self.assertEqual(n.wait(filler)["lifecycle"], "WaitingApproval")
        grant = self.grant()
        request = self.request(grant, run=1)
        scheduled = n.call("Schedule.Create", request)
        self.rejected("QueueFull", "Schedule.Create", self.request(grant))
        after(request["run_at"], 0.5)
        self.assertEqual(self.state(scheduled), scheduled)
        self.assertEqual(self.budget(grant), (0, 0))
        node = n.call("Node.Get", {})["reference"]
        cordoned = n.call("Node.Configure", dict(scheduling="Cordoned", expected_resource_version=node["version"]))
        n.call("Task.Cancel", target(n.get(filler)["reference"]))
        time.sleep(0.4)
        self.assertEqual(self.state(scheduled), scheduled)
        n.call("Node.Configure", dict(scheduling="Enabled", expected_resource_version=cordoned["version"]))
        triggered = self.wait(scheduled, "Triggered")
        self.assertEqual(n.wait(triggered["state"]["task_receipt"]["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(self.budget(grant), (0, 1))

    def test_agent_uses_its_own_scope_and_receipt(self):
        n = self.node
        agent = n.call("Agent.Register", {"name": "scheduled-inventory"})
        observer, _ = n.agent(["task.read"])
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        grant = self.grant("node.inventory", node, principal=agent["principal_id"])
        request = self.request(grant, "node.inventory")
        scheduled = n.call("Schedule.Create", request, agent["token"])
        identity = {"id": scheduled["reference"]["resource_id"]}
        self.assertEqual(n.call("Schedule.Get", identity, agent["token"]), scheduled)
        self.rejected("PermissionDenied", "Schedule.Get", identity, observer["token"])
        receipt = self.wait(scheduled, "Triggered")["state"]["task_receipt"]
        self.assertEqual(receipt["origin_ref"]["authority_id"], node)
        task = n.wait(receipt["task_id"])
        observed = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, agent["token"])
        self.assertGreater(observed["memory_bytes"]["MemTotal"], 0)
        self.rejected("PermissionDenied", "Task.Get", {"id": task["task_id"]}, observer["token"])

    def test_manual_retry_preserves_the_original_trigger_identity(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "scheduled-retry"})
        grant = self.grant("volume.write", volume["resource_id"])
        request = self.request(grant, "volume.write", dict(**target(volume), name="retry", content="once"), run=0.75)
        scheduled = n.call("Schedule.Create", request)
        original = self.wait(scheduled, "Triggered")
        task = original["state"]["task_receipt"]["task_id"]
        n.approve(task, approve=False)
        self.assertEqual(n.wait(task)["lifecycle"], "Cancelled")
        self.assertEqual(self.budget(grant), (0, 0))
        retry = n.call("Task.Retry", dict(**target(n.get(task)["reference"]), key=key()))
        self.assertEqual(retry["origin_ref"]["origin_type"], "HumanRequest")
        child = n.wait(retry["task_id"], autoapprove=True)
        self.assertEqual((child["lifecycle"], child["retry_of_task_id"]), ("Succeeded", task))
        self.assertEqual(self.state(scheduled), original)
        self.assertEqual(self.budget(grant), (0, 1))

    @unittest.skipUnless(os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION"), "requires a bounded filesystem")
    def test_waits_for_authority_filesystem_headroom(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()),
                                            authority_reserve={"state_bytes": 4 * 1024 * 1024})))
        n.start()
        grant = self.grant()
        request = self.request(grant, run=1)
        schedule = n.call("Schedule.Create", request)
        storage = n.call("Node.Get", {})["authority_storage"]["state"]
        self.assertLessEqual(int(storage["capacity_bytes"]), 64 * 1024 * 1024)
        scratch = n.base / "fill"
        with scratch.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, int(storage["available_bytes"]) - 2 * 1024 * 1024)
        self.assertTrue(n.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        after(request["run_at"], 0.5)
        self.assertEqual(self.state(schedule), schedule)
        self.assertEqual(self.budget(grant), (0, 0))
        scratch.unlink()
        triggered = self.wait(schedule, "Triggered")
        self.assertEqual(n.wait(triggered["state"]["task_receipt"]["task_id"])["lifecycle"], "Succeeded")
