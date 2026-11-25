"""Reserve, retain and evict actual per-principal read-result bytes."""
import json
import os
import signal
import unittest

from support import Failure, Node, target


RESERVATION = 4 * 1024 * 1024


class ReadResultQuotaTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.n.stop()
        settings = json.loads(self.n.config.read_text())
        settings["read_result_bytes_per_principal"] = RESERVATION
        self.n.config.write_text(json.dumps(settings))
        self.n.start()

    def scheduling(self, mode):
        ref = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})

    def usage(self, token=None):
        return self.n.call("Task.Admission", {}, token)["read_results"]

    def refuse(self, request):
        with self.assertRaises(Failure) as failure:
            self.n.call("Task.Submit", request)
        self.assertEqual(failure.exception.code, "QuotaExceeded")
        self.assertEqual(failure.exception.args[0]["retry_condition"], "SameRequestAfterBackoff")

    def test_pending_retention_and_eviction_have_one_durable_accounting_path(self):
        n = self.n
        self.scheduling("Cordoned")
        first = n.prepare("system.status", {})
        accepted = n.call("Task.Submit", first)
        self.assertEqual(self.usage(), {"reserved_bytes": str(RESERVATION), "retained_bytes": "0",
                                        "limit_bytes": str(RESERVATION),
                                        "reservation_per_task_bytes": str(RESERVATION)})
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.usage()["reserved_bytes"], str(RESERVATION))
        replay = n.call("Task.Submit", first)
        self.assertEqual(replay["task_id"], accepted["task_id"])
        self.assertTrue(replay["deduplicated"])
        next_request = n.prepare("system.status", {})
        self.refuse(next_request)
        n.call("Task.Cancel", target(n.get(accepted["task_id"])["reference"]))
        self.assertEqual(self.usage()["reserved_bytes"], "0")
        saved = n.call("Task.Submit", next_request)["task_id"]
        self.scheduling("Enabled")
        task = n.wait(saved)
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        reference, = task["result"]["artifact_refs"]
        size = n.call("Artifact.Get", {"reference": reference})["resource"]["spec"]["size_bytes"]
        self.assertGreater(int(size), 0)
        self.assertEqual(self.usage()["retained_bytes"], size)
        self.assertEqual(self.usage()["reserved_bytes"], "0")
        self.refuse(n.prepare("system.status", {}))

        actor, grant = n.agent(["task.submit", "task.read", "system.status"])
        request = n.prepare("system.status", {}, token=actor["token"],
                            delegation=grant["delegation_ref"]["resource_id"])
        peer = n.call("Task.Submit", request, actor["token"])["task_id"]
        self.assertEqual(n.wait(peer)["lifecycle"], "Succeeded")
        self.assertGreater(int(self.usage(actor["token"])["retained_bytes"]), 0)
        self.assertTrue(n.perform("volume.create", {"name": "effect-evidence"})["resource_id"].startswith("volume-"))
        self.assertEqual(self.usage()["retained_bytes"], size)

        n.call("Task.EvictResults", {**target(n.get(saved)["reference"]), "reason": "release accounted bytes"})
        self.assertEqual(self.usage()["retained_bytes"], "0")
        with self.assertRaises(Failure) as eviction:
            n.call("Artifact.Read", {"reference": reference})
        self.assertEqual(eviction.exception.code, "ResultEvicted")
        self.assertEqual(n.wait(n.submit("system.status", {}))["lifecycle"], "Succeeded")
