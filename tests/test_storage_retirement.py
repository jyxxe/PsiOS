"""Confirm filesystem deletion across lost receipts and daemon restarts."""
import os
import signal
import sqlite3
import time
import unittest

from support import Failure, Node, target


class StorageRetirementTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "retirement-source"})
        self.original = self.n.state / "volumes" / self.volume["resource_id"]
        (self.original / "data" / "business-data").write_bytes(b"persistent record")

    def trigger(self, predicate):
        with sqlite3.connect(self.n.state / "state.db", timeout=10) as database:
            database.execute(f"""CREATE TRIGGER reject_retirement_receipt BEFORE INSERT ON versions
                WHEN {predicate} BEGIN SELECT RAISE(ABORT, 'injected retirement receipt failure'); END""")

    def drop_trigger(self):
        with sqlite3.connect(self.n.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER reject_retirement_receipt")

    def operation(self, task):
        return self.n.call("Task.Get", {"id": task})["operation_refs"][0]

    def state(self, reference):
        return self.n.get(reference["resource_id"])["value"]

    def wait_for(self, predicate, complaint):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            if value := predicate():
                return value
            time.sleep(0.04)
        self.fail(complaint)

    def test_unknown_backup_delete_retains_evidence_until_admin_reconciles(self):
        n = self.n
        backup = n.perform("backup.create", target(self.volume))
        original = n.state / "backups" / backup["resource_id"]
        inode = original.stat().st_ino
        payload = (original / "blob-0").read_bytes()
        actor = n.call("Agent.Register", {"name": "retired-backup-reader"})
        n.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": ["backup.export"], "units": 1,
            "ttl_seconds": 180, "tool_targets": [backup["resource_id"]],
        })
        export = {"reference": backup, "member": {"kind": "Manifest"}, "offset": 0, "length": 64}
        n.call("Backup.Export", export, actor["token"])
        self.trigger("NEW.kind='Backup' AND NEW.body->>'lifecycle'='Deleted'")
        task_id = n.submit("backup.delete", target(backup))
        self.assertEqual(n.wait(task_id)["lifecycle"], "WaitingApproval")
        self.assertEqual(original.stat().st_ino, inode)
        task = n.wait(task_id, autoapprove=True)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Paused", "Unknown"), task)
        op = self.operation(task_id)
        retirement_id = f"storage-retirement-{op['resource_id']}"
        retired = original.parent / f".retired-{op['resource_id']}"
        self.assertEqual(self.state(backup)["lifecycle"], "Stored")
        self.assertFalse(original.exists())
        self.assertEqual(retired.stat().st_ino, inode)
        self.assertEqual(n.get(retirement_id)["value"]["lifecycle"], "Prepared")

        n.stop(signal.SIGKILL)
        self.drop_trigger()
        n.start()
        self.assertEqual(self.operation(task_id), op)
        self.assertEqual(retired.stat().st_ino, inode)
        with self.assertRaises(Failure) as held:
            n.call("Backup.Export", export, actor["token"])
        self.assertEqual(held.exception.code, "Conflict")
        with self.assertRaises(Failure) as denied:
            n.call("Operation.Reconcile", target(op), actor["token"])
        self.assertEqual(denied.exception.code, "PermissionDenied")

        legitimate = retired.rename(original.parent / "fixture-held-elsewhere")
        retired.mkdir(mode=0o700)
        with self.assertRaises(Failure) as substituted:
            n.call("Operation.Reconcile", target(op))
        self.assertEqual(substituted.exception.code, "OutcomeUnknown")
        self.assertEqual((legitimate / "blob-0").read_bytes(), payload)
        retired.rmdir()
        legitimate.rename(retired)

        completed = n.call("Operation.Reconcile", target(op))
        self.assertEqual(completed["outcome_knowledge"], "Known")
        task = n.wait(task_id)
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        deletion = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        self.assertEqual(deletion["resource_id"], backup["resource_id"])
        self.assertEqual(self.state(deletion)["lifecycle"], "Deleted")
        self.wait_for(lambda: n.get(retirement_id)["value"]["lifecycle"] == "Reclaimed",
                      "authorized retirement did not reclaim the original backup")
        self.assertFalse(retired.exists())
        self.assertEqual((self.original / "data" / "business-data").read_bytes(), b"persistent record")

    def test_retry_reclaims_volume_after_physical_cleanup_receipt_was_lost(self):
        n = self.n
        nested = self.original / "data" / "readonly"
        nested.mkdir()
        (nested / "record").write_text("retired data")
        nested.chmod(0o500)
        self.trigger("NEW.kind='StorageRetirement' AND NEW.body->>'lifecycle'='Reclaimed'")
        task_id = n.submit("volume.delete", target(self.volume))
        task = n.wait(task_id, autoapprove=True)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        result = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        reference = result["receipt"]["retirement_ref"]
        op = self.operation(task_id)
        retired = self.original.parent / f".retired-{op['resource_id']}"
        self.assertFalse(self.original.exists())
        self.wait_for(lambda: not retired.exists(), "read-only directory was not fully removed")
        self.assertEqual(self.state(reference)["lifecycle"], "PendingCleanup")
        n.stop(signal.SIGKILL)
        self.drop_trigger()
        n.start()
        self.wait_for(lambda: self.state(reference)["lifecycle"] == "Reclaimed",
                      "uncommitted reclamation did not settle after restarting")
        self.assertEqual(self.state(reference)["observation"], "DirectoryAlreadyAbsent")
        self.assertEqual(self.state(self.volume)["lifecycle"], "Deleted")
        self.assertEqual(self.operation(task_id), op)

    def test_reclamation_receipt_failure_does_not_strand_later_backup(self):
        n = self.n
        unrelated = n.perform("volume.create", {"name": "independent-backup-source"})
        source_data = n.state / "volumes" / unrelated["resource_id"] / "data" / "keep"
        source_data.write_bytes(b"independent business data")
        backup = n.perform("backup.create", target(unrelated))
        backup_directory = n.state / "backups" / backup["resource_id"]
        self.assertTrue(backup_directory.is_dir())

        blocked = self.volume["resource_id"]
        self.trigger("NEW.kind='StorageRetirement' AND NEW.body->>'lifecycle'='Reclaimed' "
                     f"AND NEW.spec->'target_ref'->>'resource_id'='{blocked}'")
        first = n.submit("volume.delete", target(self.volume))
        first_task = n.wait(first, autoapprove=True)
        self.assertEqual((first_task["lifecycle"], first_task["outcome_knowledge"]),
                         ("Succeeded", "Known"), first_task)
        first_op = self.operation(first)
        first_record = f"storage-retirement-{first_op['resource_id']}"
        first_directory = self.original.parent / f".retired-{first_op['resource_id']}"
        self.wait_for(lambda: not first_directory.exists(), "physical volume reclamation did not finish")
        self.assertEqual(n.get(first_record)["value"]["lifecycle"], "PendingCleanup")

        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.get(first_record)["value"]["lifecycle"], "PendingCleanup")
        second = n.submit("backup.delete", target(backup))
        second_task = n.wait(second, autoapprove=True)
        self.assertEqual((second_task["lifecycle"], second_task["outcome_knowledge"]),
                         ("Succeeded", "Known"), second_task)
        second_op = self.operation(second)
        second_record = f"storage-retirement-{second_op['resource_id']}"
        second_directory = backup_directory.parent / f".retired-{second_op['resource_id']}"
        self.wait_for(lambda: n.get(second_record)["value"]["lifecycle"] == "Reclaimed",
                      "the failed first receipt prevented independent backup reclamation")
        self.assertFalse(backup_directory.exists())
        self.assertFalse(second_directory.exists())
        self.assertEqual(n.get(first_record)["value"]["lifecycle"], "PendingCleanup")
        self.assertFalse(first_directory.exists())
        self.assertEqual(source_data.read_bytes(), b"independent business data")

        self.drop_trigger()
        self.wait_for(lambda: n.get(first_record)["value"]["lifecycle"] == "Reclaimed",
                      "the original durable volume receipt did not settle after removing the fault")
        self.assertEqual(n.get(first_record)["value"]["observation"], "DirectoryAlreadyAbsent")
        self.assertEqual(self.operation(first), first_op)
        self.assertEqual(self.operation(second), second_op)


if __name__ == "__main__":
    unittest.main()
