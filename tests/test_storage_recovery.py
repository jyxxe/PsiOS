"""Process loss during real disk copies, before a new resource can be published."""
import hashlib
import os
import resource
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, target


class StorageRecoveryTests(unittest.TestCase):
    limit = 2 * 1024 * 1024

    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)
        self.volumes = self.n.state / "volumes"
        self.backups = self.n.state / "backups"
        self.volume = self.n.perform("volume.create", {"name": "transaction-source"})
        self.source = self.volumes / self.volume["resource_id"] / "data" / "record"
        self.source.write_bytes(b"pre-publication filesystem recovery\n" * 128000)
        with self.source.open("rb") as file:
            self.digest = hashlib.file_digest(file, "sha256").hexdigest()
        self.backup = self.n.perform("backup.create", target(self.volume))

    def interrupt_copy(self, tool, reference, parent, incomplete):
        resource.prlimit(self.n.process.pid, resource.RLIMIT_FSIZE, (self.limit, self.limit))
        task_id = self.n.submit(tool, target(reference))
        self.assertEqual(self.n.process.wait(20), -signal.SIGXFSZ)
        stages = list(parent.glob(".stage-*"))
        self.assertEqual(len(stages), 1)
        self.assertEqual((stages[0] / incomplete).stat().st_size, self.limit)
        return task_id, stages[0]

    def assert_unpublished_restore(self, task_id):
        task = self.n.call("Task.Get", {"id": task_id})
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Failed", "Known"))
        self.assertEqual(task["conditions"][0]["type"], "LeaseLost")
        self.assertEqual(task["result"]["artifact_refs"], [])

    def test_restart_reclaims_interrupted_backup_and_legacy_stage(self):
        task, stage = self.interrupt_copy("backup.create", self.volume, self.backups, "blob-0")
        old_stage = stage.rename(self.backups / ".tmpA1b2c3")
        retained = self.backups / "backup-operation-awaiting-receipt"
        retained.mkdir(mode=0o700)
        (retained / "manifest.json").write_text("unregistered published path")
        link = self.backups / ".stage-AA001b"
        link.symlink_to(retained, target_is_directory=True)
        subprocess.run(self.n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL)
        self.assertTrue(old_stage.exists())

        self.n.start()
        self.assertFalse(old_stage.exists())
        self.assertTrue(link.is_symlink())
        self.assertEqual((retained / "manifest.json").read_text(), "unregistered published path")
        grant = self.n.call("Grant.Issue", dict(principal_id="local-owner", units=1, ttl_seconds=120,
            actions=["task.submit", "task.read", "backup.verify"], tool_targets=[self.backup["resource_id"]]))
        request = self.n.prepare("backup.verify", target(self.backup), delegation=grant["delegation_ref"]["resource_id"])
        checked = self.n.wait(self.n.call("Task.Submit", request)["task_id"])
        self.assertEqual(checked["lifecycle"], "Succeeded", checked)
        artifact = self.n.call("Artifact.Read", {"reference": checked["result"]["artifact_refs"][0]})
        self.assertEqual(artifact["bytes"], str(self.source.stat().st_size))
        interrupted = self.n.call("Task.Get", {"id": task})
        self.assertEqual((interrupted["lifecycle"], interrupted["outcome_knowledge"]), ("Failed", "Known"))
        self.assertEqual(interrupted["conditions"][0]["type"], "LeaseLost")
        self.assertEqual(interrupted["result"]["artifact_refs"], [])
        self.assertNotEqual(self.n.perform("backup.create", target(self.volume)), self.backup)

    def test_restart_reclaims_interrupted_restore_and_readonly_nested_stage(self):
        backup = self.backups / self.backup["resource_id"]
        manifest = (backup / "manifest.json").read_bytes()
        task, stage = self.interrupt_copy("backup.restore", self.backup, self.volumes, "data/record")
        nested = stage / "data" / "readonly"
        nested.mkdir(mode=0o700)
        (nested / "local").write_text("staged data")
        (nested / "outside").symlink_to(self.source)
        nested.chmod(0o555)
        unrelated = self.volumes / ".stage-operator-directory"
        unrelated.mkdir(mode=0o700)

        self.n.start()
        self.assertFalse(stage.exists())
        self.assertTrue(unrelated.exists())
        self.assertEqual({p.name for p in self.volumes.glob("volume-*")}, {self.volume["resource_id"]})
        with self.source.open("rb") as file:
            self.assertEqual(hashlib.file_digest(file, "sha256").hexdigest(), self.digest)
        self.assertEqual((backup / "manifest.json").read_bytes(), manifest)
        self.assert_unpublished_restore(task)
        self.n.stop()
        self.n.start()
        self.assertFalse(stage.exists())
        self.assert_unpublished_restore(task)
        restored = self.n.perform("backup.restore", target(self.backup))
        copied = self.volumes / restored["resource_id"] / "data" / "record"
        with copied.open("rb") as file:
            self.assertEqual(hashlib.file_digest(file, "sha256").hexdigest(), self.digest)

    def test_restore_write_failure_reclaims_readonly_stage_without_restart(self):
        nested = self.source.parent / "readonly"
        nested.mkdir(mode=0o700)
        with (nested / "large").open("wb") as file:
            file.truncate(32 * 1024 * 1024)
        nested.chmod(0o555)
        backup = self.n.perform("backup.create", target(self.volume))
        task_id = self.n.submit("backup.restore", target(backup))
        stop_at = time.monotonic() + 15
        while True:
            partial = next(self.volumes.glob(".stage-*/data/readonly/large"), None)
            if partial is not None:
                break
            self.assertLess(time.monotonic(), stop_at, "restore stage did not appear")
            time.sleep(0.001)

        self.n.process.send_signal(signal.SIGSTOP)
        try:
            _, stopped = os.waitpid(self.n.process.pid, os.WUNTRACED)
            self.assertTrue(os.WIFSTOPPED(stopped))
            stage = partial.parents[2]
            self.assertTrue(partial.exists())
            # The real new-file write after copying and applying restored modes must fail.
            (stage / "identity").touch(exist_ok=False)
        finally:
            self.n.process.send_signal(signal.SIGCONT)
        failed = self.n.wait(task_id)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "Known"))
        self.assertEqual(failed["conditions"][0]["type"], "StorageUnavailable", failed)
        self.assertEqual(list(self.volumes.glob(".stage-*")), [])
        self.assertIsNone(self.n.process.poll())
        restored = self.n.perform("backup.restore", target(backup))
        data = self.volumes / restored["resource_id"] / "data" / "readonly"
        self.assertEqual(data.stat().st_mode & 0o777, 0o555)
        self.assertEqual((data / "large").stat().st_size, 32 * 1024 * 1024)

    def test_activation_verifies_restored_tree_after_restart_and_backup_deletion(self):
        clone = self.n.perform("backup.restore", target(self.backup))
        data = self.volumes / clone["resource_id"] / "data"
        record = data / "record"
        original = record.read_bytes()
        stamp = record.stat()
        root_stamp = data.stat()
        self.n.perform("backup.delete", target(self.backup))
        self.n.stop()
        record.write_bytes(b"P" + original[1:])
        os.utime(record, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.n.start()

        def refuses_activation():
            task = self.n.wait(self.n.submit("volume.activate", target(clone)), autoapprove=True)
            self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Failed", "Known"))
            self.assertEqual(task["conditions"][0]["type"], "PreconditionFailed", task)
            pending = self.n.get(clone["resource_id"])
            self.assertEqual(pending["reference"], clone)
            self.assertEqual(pending["resource"]["spec"]["storage_restore_phase"], "AwaitingActivation")

        refuses_activation()
        record.write_bytes(original)
        os.utime(record, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        extra = data / "unexpected"
        extra.touch()
        os.utime(data, ns=(root_stamp.st_atime_ns, root_stamp.st_mtime_ns))
        refuses_activation()
        extra.unlink()
        os.utime(data, ns=(root_stamp.st_atime_ns, root_stamp.st_mtime_ns))

        activated = self.n.perform("volume.activate", target(clone))
        spec = self.n.get(activated["resource_id"])["resource"]["spec"]
        self.assertEqual(spec["storage_restore_phase"], "Activated")
        evidence = self.n.get(spec["restore_verification_ref"]["resource_id"])
        self.assertEqual(evidence["reference"], spec["restore_verification_ref"])
        self.assertEqual(evidence["value"]["tree_digest"], spec["restore_proof"]["tree_digest"])
        self.assertEqual(evidence["value"]["volume_ref"], clone)
        self.assertEqual(evidence["value"]["backup_ref"], self.backup)

    def test_restore_reconciliation_verifies_original_volume_before_cataloging(self):
        n = self.n
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("""CREATE TRIGGER lose_restore_receipt BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.spec->>'storage_restore_phase'='AwaitingActivation'
                BEGIN SELECT RAISE(ABORT, 'injected restored volume receipt loss'); END""")
        task_id = n.submit("backup.restore", target(self.backup))
        paused = n.wait(task_id)
        self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
        op = paused["operation_refs"][0]
        volume_id = f"volume-{op['resource_id']}"
        candidate = self.volumes / volume_id / "data" / "record"
        inode = candidate.stat().st_ino
        with self.assertRaises(Failure) as unregistered:
            n.get(volume_id)
        self.assertEqual(unregistered.exception.code, "NotFound")
        n.stop(signal.SIGKILL)
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER lose_restore_receipt")
        n.start()
        self.assertEqual(n.wait(task_id), paused)
        self.assertEqual(candidate.stat().st_ino, inode)
        refused_deletion = n.wait(n.submit("backup.delete", target(self.backup)), autoapprove=True)
        self.assertEqual(refused_deletion["lifecycle"], "Failed")
        self.assertEqual(refused_deletion["conditions"][0]["type"], "Conflict")

        inspection = "volume.inspect_publication"
        grant = {"principal_id": "local-owner", "units": 5, "ttl_seconds": 180,
                 "actions": [inspection, "task.submit", "task.read"]}
        with self.assertRaises(Failure) as unscoped:
            n.call("Grant.Issue", grant)
        self.assertEqual(unscoped.exception.code, "InvalidArgument")
        allowed = n.call("Grant.Issue", {**grant, "tool_targets": [op["resource_id"]]})

        def inspect():
            request = n.prepare(inspection, target(op), delegation=allowed["delegation_ref"]["resource_id"])
            return n.wait(n.call("Task.Submit", request)["task_id"])

        first = inspect()
        self.assertEqual(first["lifecycle"], "Succeeded", first)
        fresh = first["result"]["artifact_refs"][0]
        self.assertEqual(n.call("Artifact.Read", {"reference": fresh})["operation_ref"], op)
        inspected = candidate.stat()
        with candidate.open("r+b") as file:
            original = file.read(1)
            file.seek(0)
            file.write(b"P")
        os.utime(candidate, ns=(inspected.st_atime_ns, inspected.st_mtime_ns))
        with self.assertRaises(Failure) as stale:
            n.call("Operation.Reconcile", {**target(op), "evidence_ref": fresh})
        self.assertEqual(stale.exception.code, "OutcomeUnknown")
        rejected = inspect()
        self.assertEqual(rejected["lifecycle"], "Failed", rejected)
        self.assertEqual(rejected["conditions"][0]["type"], "PreconditionFailed")
        self.assertEqual(n.wait(task_id)["outcome_knowledge"], "Unknown")

        with candidate.open("r+b") as file:
            file.write(original)
        os.utime(candidate, ns=(inspected.st_atime_ns, inspected.st_mtime_ns))
        renewed = inspect()
        self.assertEqual(renewed["lifecycle"], "Succeeded", renewed)
        reconciled = n.call("Operation.Reconcile", {
            **target(op), "evidence_ref": renewed["result"]["artifact_refs"][0],
        })
        self.assertEqual(reconciled["outcome_knowledge"], "Known")
        final = n.wait(task_id)
        self.assertEqual(final["lifecycle"], "Succeeded", final)
        volume = n.call("Artifact.Read", {"reference": final["result"]["artifact_refs"][0]})
        self.assertEqual(volume["resource_id"], volume_id)
        self.assertEqual(candidate.stat().st_ino, inode)
        spec = n.get(volume_id)["resource"]["spec"]
        self.assertEqual(spec["restored_from"], self.backup)
        self.assertEqual(spec["storage_restore_phase"], "AwaitingActivation")
        write = n.wait(n.submit("volume.write", {**target(volume), "name": "must-wait", "content": "x"}),
                       autoapprove=True)
        self.assertEqual(write["conditions"][0]["type"], "PreconditionFailed")
        self.assertFalse((candidate.parent / "must-wait").exists())
        activated = n.perform("volume.activate", target(volume))
        self.assertEqual(n.get(activated["resource_id"])["resource"]["spec"]["storage_restore_phase"], "Activated")


if __name__ == "__main__":
    unittest.main()
