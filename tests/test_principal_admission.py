"""Task admission quotas apply to the authenticated principal across independent grants."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import unittest

from support import Failure, Node, target
from test_recovery_admission import lose_restore_receipt


class PrincipalAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)

    def limits(self, per_principal):
        n = self.n
        n.stop()
        settings = json.loads(n.config.read_text())
        settings.update(queue_limit=4, principal_queue_limit=per_principal, recovery_queue_limit=1)
        n.config.write_text(json.dumps(settings))
        n.start()

    def mode(self, scheduling):
        n = self.n
        ref = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": scheduling})

    def test_repeated_grants_share_admission_and_other_principals_keep_their_slots(self):
        n = self.n
        self.limits(2)
        self.mode("Cordoned")
        actions = ["task.submit", "task.read", "system.status"]
        alice, grant_a = n.agent(actions)
        bob, grant_b = n.agent(actions)
        other_a = n.call("Grant.Issue", {
            "principal_id": alice["principal_id"], "actions": actions,
            "units": 4, "ttl_seconds": 120,
        })

        def prepare(actor, grant):
            return n.prepare("system.status", {}, token=actor["token"],
                             delegation=grant["delegation_ref"]["resource_id"])

        def submit(actor, request):
            return n.call("Task.Submit", request, actor["token"])

        def assert_usage(actor, expected):
            admission = n.call("Task.Admission", {}, actor["token"])
            self.assertEqual(admission, {"principal_id": actor["principal_id"],
                                         "ordinary": {"used": expected, "limit": 2}})

        request = prepare(alice, grant_a)
        first = submit(alice, request)["task_id"]
        competing = [prepare(alice, grant_a), prepare(alice, other_a)]

        def race(item):
            try:
                return submit(alice, item)
            except Failure as rejected:
                self.assertEqual(rejected.code, "QueueFull")
                self.assertIn("principal", str(rejected).lower())
                return None

        with ThreadPoolExecutor(max_workers=2) as pool:
            receipts = list(pool.map(race, competing))
        winner, = [receipt["task_id"] for receipt in receipts if receipt is not None]
        retry, = [params for params, receipt in zip(competing, receipts) if receipt is None]
        assert_usage(alice, 2)
        assert_usage(bob, 0)
        repeated = submit(alice, request)
        self.assertEqual(repeated["task_id"], first)
        self.assertTrue(repeated["deduplicated"])
        bob_task = submit(bob, prepare(bob, grant_b))["task_id"]
        self.assertEqual(n.call("Node.Get", {})["admission"]["ordinary"], {"used": 3, "limit": 4})
        assert_usage(bob, 1)

        user_token = n.base / "reader.token"
        user_token.write_text(alice["token"])
        user_token.chmod(0o600)
        self.assertEqual(n.cli(user_token, "task-admission")["ordinary"]["used"], 2)
        outsider = n.call("Agent.Register", {"name": "no-admission-scope"})
        with self.assertRaises(Failure) as denied:
            n.call("Task.Admission", {}, outsider["token"])
        self.assertEqual(denied.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as foreign:
            n.call("Task.Admission", {"principal_id": bob["principal_id"]}, alice["token"])
        self.assertEqual(foreign.exception.code, "InvalidArgument")

        n.stop(signal.SIGKILL)
        n.start()
        assert_usage(alice, 2)
        assert_usage(bob, 1)
        n.call("Task.Cancel", target(n.get(first)["reference"]))
        assert_usage(alice, 1)
        accepted = submit(alice, retry)
        self.assertFalse(accepted["deduplicated"])
        assert_usage(alice, 2)
        self.mode("Enabled")
        for task in (winner, bob_task, accepted["task_id"]):
            self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
        assert_usage(alice, 0)
        assert_usage(bob, 0)

    def test_unresolved_effect_keeps_the_principal_slot_and_can_use_the_recovery_reserve(self):
        n = self.n
        self.limits(1)
        volume = n.perform("volume.create", {"name": "admission-recovery-source"})
        backup = n.perform("backup.create", target(volume))
        task, unknown = lose_restore_receipt(n, backup)
        self.assertEqual(unknown["outcome_knowledge"], "Unknown")
        self.assertEqual(n.call("Task.Admission", {})["ordinary"], {"used": 1, "limit": 1})
        with self.assertRaises(Failure) as saturated:
            n.submit("system.status", {})
        self.assertEqual(saturated.exception.code, "QueueFull")
        self.assertIn("principal", str(saturated.exception).lower())
        other, budget = n.agent(["task.submit", "task.read", "system.status"])
        ready = n.prepare("system.status", {}, token=other["token"],
                          delegation=budget["delegation_ref"]["resource_id"])
        accepted = n.call("Task.Submit", ready, other["token"])["task_id"]
        self.assertEqual(n.wait(accepted)["lifecycle"], "Succeeded")

        operation, = unknown["operation_refs"]
        tool = "volume.inspect_publication"
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", tool],
            "units": 2, "ttl_seconds": 180, "tool_targets": [operation["resource_id"]],
        })
        self.mode("Cordoned")
        inspect = n.prepare(tool, target(operation), delegation=grant["delegation_ref"]["resource_id"])
        observer = n.call("Task.Submit", inspect)["task_id"]
        self.assertEqual(n.call("Node.Get", {})["admission"]["recovery"], {"used": 1, "limit": 1})
        self.assertEqual(n.call("Task.Admission", {})["ordinary"]["used"], 1)
        self.mode("Enabled")
        proof, = n.wait(observer)["result"]["artifact_refs"]
        n.call("Operation.Reconcile", {**target(operation), "evidence_ref": proof})
        self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
        self.assertEqual(n.call("Task.Admission", {})["ordinary"]["used"], 0)
