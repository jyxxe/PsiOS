"""Schedule real Tasks from authority-owned source revisions."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import signal
import sqlite3
import time
import unittest

from support import Failure, Node, key, target
from test_schedule import timestamp


class ResourceEventSchedules(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)

    def mode(self, scheduling):
        n = self.node
        node = n.call("Node.Get", {})["reference"]
        return n.call("Node.Configure", {
            "expected_resource_version": node["version"], "scheduling": scheduling,
        })

    def grant(self, tool, resource, principal="local-owner", event_read=True):
        actions = ["schedule.manage", "task.submit", "task.read", "resource.read", tool]
        if event_read:
            actions.append("event.read")
        return self.node.call("Grant.Issue", {
            "principal_id": principal, "actions": actions, "tool_targets": [resource],
            "units": 4, "ttl_seconds": 90,
        })

    def request(self, grant, source, *, limit=1, expiry=25, condition=None, tool="system.status", input=None):
        return {"key": key(), "tool": tool, "input": input or {},
                "delegation_id": grant["delegation_ref"]["resource_id"],
                "event": {"source_ref": source, "condition": condition or {"kind": "SpecChanged"},
                          "expires_at": timestamp(expiry), "max_triggers": limit}}

    def state(self, rule):
        return self.node.call("Schedule.Get", {"id": rule["reference"]["resource_id"]})

    def wait(self, rule, predicate):
        deadline = time.monotonic() + 10
        while True:
            state = self.state(rule)
            if predicate(state["state"]):
                return state
            self.assertLess(time.monotonic(), deadline, state)
            time.sleep(.05)

    def triggers(self, rule):
        return [self.node.call("Schedule.TriggerGet", {"id": r["resource_id"]})["state"]
                for r in self.state(rule)["state"]["trigger_refs"]]

    def budget(self, grant):
        data = self.node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        return (data["reserved"], data["spent"])

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as e:
            self.node.call(method, params, token)
        self.assertEqual(e.exception.code, code, e.exception)

    def fill_queue(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "event-reserve"})
        cfg = json.loads(n.config.read_text())
        n.stop()
        n.config.write_text(json.dumps(dict(cfg, queue_limit=1)))
        n.start()
        blocker = n.submit("volume.write", {**target(volume), "name": "hold", "content": "pending"})
        blocked = n.wait(blocker)
        self.assertEqual(blocked["lifecycle"], "WaitingApproval")
        return volume, blocked

    def pending(self, rule):
        state = self.wait(rule, lambda s: s["pending_trigger_ref"] is not None)
        reference = state["state"]["pending_trigger_ref"]
        result = self.node.call("Schedule.TriggerGet", {"id": reference["resource_id"]})
        self.assertEqual(result["reference"], reference)
        self.assertEqual(result["state"]["lifecycle"], "Pending")
        self.assertIsNone(result["state"]["task_receipt"])
        return state, result

    def wait_source_revision(self, rule, reference):
        deadline = time.monotonic() + 5
        with closing(sqlite3.connect(self.node.state / "state.db")) as db:
            while True:
                observed, = db.execute("SELECT source_rev FROM pending_event_schedules WHERE schedule=?",
                                       (rule["reference"]["resource_id"],)).fetchone()
                if observed >= int(reference["version"][1:]):
                    return
                self.assertLess(time.monotonic(), deadline, observed)
                time.sleep(.05)

    def test_task_lifecycle_event_keeps_original_approval_and_deduplicates_on_restart(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "event-data"})
        self.mode("Cordoned")
        source_id = n.submit("system.status", {})
        source = n.get(source_id)["reference"]
        grant = self.grant("volume.write", volume["resource_id"])
        request = self.request(grant, source, limit=2, tool="volume.write",
                               input={**target(volume), "name": "event-proof", "content": "recorded"},
                               condition={"kind": "LifecycleEntered", "lifecycle": "Succeeded"})
        with ThreadPoolExecutor(max_workers=4) as callers:
            rules = list(callers.map(lambda _: n.call("Schedule.Create", request), range(4)))
        rule = rules[0]
        self.assertTrue(all(item == rule for item in rules))
        spec = n.get(rule["reference"]["resource_id"])["resource"]["spec"]
        self.denied("PermissionDenied", "Task.Submit", spec["request"])
        self.denied("InvalidArgument", "Schedule.Create", dict(request, run_at=timestamp(1)))
        self.assertEqual(self.budget(grant), (0, 0))
        cursor = n.call("Resource.List", {"kind": "Task", "id": source_id})["watch_start_cursor"]

        self.mode("Enabled")
        source_task = n.wait(source_id)
        self.assertEqual(source_task["lifecycle"], "Succeeded")
        self.wait(rule, lambda s: len(s["trigger_refs"]) == 1)
        trigger, = self.triggers(rule)
        task_id = trigger["task_receipt"]["task_id"]
        self.assertEqual(trigger["task_receipt"]["idempotency_key"], trigger["trigger_id"])
        self.assertEqual(trigger["task_receipt"]["origin_ref"]["origin_id"], rule["reference"]["resource_id"])
        self.assertEqual(n.call("Task.Get", {"id": task_id})["lifecycle"], "WaitingApproval")
        self.assertEqual(self.budget(grant), (1, 0))
        events = n.call("Event.Poll", {"cursor": cursor})["events"]
        cause = next(x for x in events if x["event_id"] == trigger["source_event"]["event_id"])
        self.assertEqual(cause["resource_ref"], n.get(source_id)["reference"])
        self.assertEqual(cause["source_seq"], trigger["source_event"]["source_seq"])
        data = n.state / "volumes" / volume["resource_id"] / "data" / "event-proof"
        self.assertFalse(data.exists())

        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.triggers(rule), [trigger])
        self.assertEqual(n.call("Schedule.Create", request), self.state(rule))
        self.denied("PreconditionFailed", "Schedule.Create", dict(request, key=key()))
        n.call("Task.EvictResults", {
            "id": source_id, "expected_resource_version": source_task["task_revision"], "reason": "test",
        })
        self.wait_source_revision(rule, n.get(source_id)["reference"])
        self.assertEqual(self.triggers(rule), [trigger])
        n.approve(task_id)
        self.assertEqual(n.wait(task_id)["lifecycle"], "Succeeded")
        self.assertEqual(data.read_text(), "recorded")
        self.assertEqual(self.budget(grant), (0, 1))
        revision = self.state(rule)["reference"]
        cancelled = n.call("Schedule.Cancel", target(revision))
        self.assertEqual(cancelled["state"]["lifecycle"], "Cancelled")
        self.assertEqual(self.triggers(rule), [trigger])

    def test_queue_pressure_retries_original_event_and_skips_overlap(self):
        n = self.node
        volume, blocked = self.fill_queue()
        grant = self.grant("volume.write", volume["resource_id"])
        beginning = n.call("Node.Get", {})["reference"]
        rule = n.call("Schedule.Create", self.request(
            grant, beginning, limit=2, tool="volume.write",
            input={**target(volume), "name": "next", "content": "exactly once"},
        ))
        cordoned = self.mode("Cordoned")
        enabled = self.mode("Enabled")
        _, pending = self.pending(rule)
        self.assertEqual(pending["state"]["source_event"]["resource_ref"], cordoned)
        self.assertEqual(self.triggers(rule), [])
        self.assertEqual(self.budget(grant), (0, 0))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.triggers(rule), [])
        self.assertEqual(self.pending(rule)[1], pending)
        n.call("Task.Cancel", {"id": blocked["task_id"], "expected_resource_version": blocked["task_revision"]})
        self.wait(rule, lambda s: s["lifecycle"] == "Completed")
        first, second = self.triggers(rule)
        self.assertEqual((first["lifecycle"], second["lifecycle"]), ("Triggered", "Skipped"))
        self.assertEqual(first["trigger_id"], pending["state"]["trigger_id"])
        self.assertEqual(first["source_event"], pending["state"]["source_event"])
        self.assertEqual((first["source_event"]["resource_ref"], second["source_event"]["resource_ref"]),
                         (cordoned, enabled))
        self.assertEqual(second["reason"], "PreviousTaskActive")
        self.assertIsNone(second["task_receipt"])
        self.assertEqual(self.budget(grant), (1, 0))
        child = first["task_receipt"]["task_id"]
        n.approve(child)
        self.assertEqual(n.wait(child)["lifecycle"], "Succeeded")
        self.assertEqual(self.budget(grant), (0, 1))
        data = n.state / "volumes" / volume["resource_id"] / "data"
        self.assertEqual((data / "next").read_text(), "exactly once")
        self.assertFalse((data / "hold").exists())

    def test_pending_trigger_records_expiry_and_cancel_without_admission(self):
        n = self.node
        _, blocked = self.fill_queue()
        grant = self.grant("system.status", "profile-local")
        for phase, deadline in (("Expired", 3), ("Cancelled", 25)):
            with self.subTest(phase=phase):
                start = n.call("Node.Get", {})["reference"]
                rule = n.call("Schedule.Create", self.request(grant, start, expiry=deadline))
                revision = self.mode("Cordoned")
                self.mode("Enabled")
                staged, pending = self.pending(rule)
                self.assertEqual(pending["state"]["source_event"]["resource_ref"], revision)
                if phase == "Cancelled":
                    n.call("Schedule.Cancel", target(staged["reference"]))
                final = self.wait(rule, lambda s: s["lifecycle"] == phase)
                self.assertIsNone(final["state"]["pending_trigger_ref"])
                settled, = self.triggers(rule)
                self.assertEqual((settled["trigger_id"], settled["lifecycle"]),
                                 (pending["state"]["trigger_id"], phase))
                self.assertIsNone(settled["task_receipt"])
                self.assertEqual(settled["source_event"], pending["state"]["source_event"])
                self.assertEqual(self.budget(grant), (0, 0))
        self.assertEqual(n.call("Task.Get", {"id": blocked["task_id"]}), blocked)

    def test_scope_rechecked_and_expiration_settles_while_cordoned(self):
        n = self.node
        actor = n.call("Agent.Register", {"name": "event-author"})
        delegated = self.grant("system.status", "profile-local", actor["principal_id"])
        no_event = self.grant("system.status", "profile-local", actor["principal_id"], event_read=False)
        source = self.mode("Cordoned")
        request = self.request(delegated, source)
        self.denied("PermissionDenied", "Schedule.Create",
                    dict(request, delegation_id=no_event["delegation_ref"]["resource_id"]), actor["token"])
        other_id = n.submit("system.status", {})
        self.denied("PermissionDenied", "Schedule.Create",
                    dict(request, event=dict(request["event"], source_ref=n.get(other_id)["reference"])), actor["token"])
        ruled = n.call("Schedule.Create", request, actor["token"])
        scope = self.grant("system.status", "profile-local")
        expires = n.call("Schedule.Create", self.request(scope, source, expiry=1))
        expired = self.wait(expires, lambda s: s["lifecycle"] == "Expired")
        self.assertEqual(expired["state"]["trigger_refs"], [])
        self.assertEqual(self.budget(scope), (0, 0))
        n.call("Grant.Revoke", target(delegated["grant_ref"]))
        self.mode("Enabled")
        rejected = self.wait(ruled, lambda s: s["lifecycle"] == "Rejected")
        self.assertEqual(rejected["state"]["reason"], "PermissionDenied")
        self.assertEqual(rejected["state"]["trigger_refs"], [])
        self.assertEqual(self.budget(delegated), (0, 0))
