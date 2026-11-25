"""A saturated durable queue still admits scoped proof for a real unresolved restore."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import sqlite3
import unittest

from support import Failure, Node, key, target


def lose_restore_receipt(node, backup):
    with sqlite3.connect(node.state / "state.db", timeout=10) as database:
        database.execute("""CREATE TRIGGER withhold_restore_catalog BEFORE INSERT ON versions
            WHEN NEW.kind='Volume' AND NEW.spec->>'storage_restore_phase'='AwaitingActivation'
            BEGIN SELECT RAISE(ABORT, 'withhold restore catalog receipt'); END""")
    try:
        task = node.submit("backup.restore", target(backup))
        return task, node.wait(task)
    finally:
        with sqlite3.connect(node.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER withhold_restore_catalog")


class RecoveryAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)

    def configure(self, recovery):
        n = self.n
        n.stop()
        settings = json.loads(n.config.read_text())
        settings.update(queue_limit=1, recovery_queue_limit=recovery)
        n.config.write_text(json.dumps(settings))
        n.start()

    def scheduling(self, mode):
        n = self.n
        ref = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})

    def denied(self, code, request):
        with self.assertRaises(Failure) as failure:
            self.n.call("Task.Submit", request)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def test_full_queue_recovers_restore_with_one_durable_proof_per_operation(self):
        n = self.n
        self.configure(0)
        volume = n.perform("volume.create", {"name": "full-queue-source"})
        data = n.state / "volumes" / volume["resource_id"] / "data"
        written = b"retained past queue saturation\n" * 8192
        (data / "record").write_bytes(written)
        backup_task = n.submit("backup.create", target(volume))
        backup_ref = n.wait(backup_task)["result"]["artifact_refs"][0]
        backup = n.call("Artifact.Read", {"reference": backup_ref})
        original, unresolved = lose_restore_receipt(n, backup)
        self.assertEqual((unresolved["lifecycle"], unresolved["outcome_knowledge"]), ("Paused", "Unknown"))
        operation = unresolved["operation_refs"][0]
        restored_id = f"volume-{operation['resource_id']}"
        candidate = n.state / "volumes" / restored_id / "data" / "record"
        original_inode = candidate.stat().st_ino
        self.assertEqual(candidate.read_bytes(), written)

        tool = "volume.inspect_publication"
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", tool],
            "tool_targets": [operation["resource_id"]], "units": 4, "ttl_seconds": 180,
        })
        delegation = grant["delegation_ref"]["resource_id"]

        def prepare_probe():
            return n.prepare(tool, target(operation), delegation=delegation)

        self.denied("QueueFull", prepare_probe())
        self.configure(2)
        self.scheduling("Cordoned")
        expected = {"ordinary": {"used": 1, "limit": 1}, "recovery": {"used": 0, "limit": 2}}
        self.assertEqual(n.call("Node.Get", {})["admission"], expected)
        self.denied("QueueFull", n.prepare("system.status", {}))
        with self.assertRaises(Failure) as unscoped:
            n.prepare(tool, target(operation))
        self.assertEqual(unscoped.exception.code, "PermissionDenied")
        with_dependency = n.call("Task.Prepare", {
            "key": key(), "tool": tool, "input": target(operation), "delegation_id": delegation,
            "requires": [{"task_id": backup_task, "digest": backup_ref["digest"]}],
        })
        self.denied("QueueFull", with_dependency)

        def try_probe(request):
            try:
                return n.call("Task.Submit", request)
            except Failure as refused:
                self.assertEqual(refused.code, "QueueFull")
                return None

        requests = [prepare_probe(), prepare_probe()]
        with ThreadPoolExecutor(max_workers=2) as pool:
            receipts = list(pool.map(try_probe, requests))
        (submitted,) = [(request, receipt) for request, receipt in zip(requests, receipts) if receipt]
        request, receipt = submitted
        queued = receipt["task_id"]
        self.assertTrue(n.call("Task.Submit", request)["deduplicated"])
        expected["recovery"]["used"] = 1
        self.assertEqual(n.call("Node.Get", {})["admission"], expected)
        n.call("Task.Cancel", target(n.get(queued)["reference"]))
        expected["recovery"]["used"] = 0
        self.assertEqual(n.call("Node.Get", {})["admission"], expected)

        next_probe = n.call("Task.Submit", prepare_probe())["task_id"]
        admission = n.get(f"recovery-admission-{next_probe}")["value"]
        self.assertEqual(admission["unresolved_operation_ref"], operation)
        n.stop(signal.SIGKILL)
        n.start()
        expected["recovery"]["used"] = 1
        self.assertEqual(n.call("Node.Get", {})["admission"], expected)
        self.assertEqual(n.wait(original), unresolved)
        self.scheduling("Enabled")
        proof = n.wait(next_probe)
        self.assertEqual(proof["lifecycle"], "Succeeded", proof)
        evidence = proof["result"]["artifact_refs"][0]
        expected["recovery"]["used"] = 0
        self.assertEqual(n.call("Node.Get", {})["admission"], expected)

        self.scheduling("Cordoned")
        superseded = n.call("Task.Submit", prepare_probe())["task_id"]
        self.assertEqual(n.call("Operation.Reconcile", {**target(operation), "evidence_ref": evidence})["outcome_knowledge"], "Known")
        self.scheduling("Enabled")
        stale = n.wait(superseded)
        self.assertEqual((stale["lifecycle"], stale["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertNotIn("current_run_id", stale)
        self.assertEqual(stale["conditions"][0]["type"], "Conflict")
        self.assertEqual(n.wait(original)["lifecycle"], "Succeeded")
        self.assertEqual(candidate.stat().st_ino, original_inode)
        self.assertEqual(candidate.read_bytes(), written)
        expected["ordinary"]["used"] = 0
        self.assertEqual(n.call("Node.Get", {})["admission"], expected)
        self.assertEqual(n.perform("system.status", {})["authority_id"], n.call("Principal.WhoAmI", {})["authority_id"])
