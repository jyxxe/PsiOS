"""Process loss during real disk copies, before a new resource can be published."""
from contextlib import closing
import hashlib
import os
import resource
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, target


class StorageRecoveryTests(unittest.TestCase):
    copy_limit = 8 * 1024 * 1024
    source_size = copy_limit + 1024 * 1024

    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volumes = self.n.state / "volumes"
        self.backups = self.n.state / "backups"
        self.volume = self.n.perform("volume.create", {"name": "transaction-source"})
        self.source = self.volumes / self.volume["resource_id"] / "data" / "record"
        pattern = b"pre-publication filesystem recovery\n"
        repetitions, tail = divmod(self.source_size, len(pattern))
        with self.source.open("wb") as file:
            file.write(pattern * repetitions)
            file.write(pattern[:tail])
        with self.source.open("rb") as file:
            self.digest = hashlib.file_digest(file, "sha256").hexdigest()
        self.backup = self.n.perform("backup.create", target(self.volume))

    def interrupt_copy(self, tool, reference, parent, incomplete):
        def authority_file_sizes():
            return {path.name: path.stat().st_size for path in self.n.state.glob("state.db*")}

        # RLIMIT_FSIZE also covers SQLite's WAL. Close the setup connection to clear
        # its WAL, and leave room for the fixture's admission and dispatch writes.
        self.n.stop()
        self.n.start()
        sizes = authority_file_sizes()
        self.assertTrue(sizes and all(size < self.copy_limit // 2 for size in sizes.values()), sizes)

        resource.prlimit(self.n.process.pid, resource.RLIMIT_FSIZE, (self.copy_limit, self.copy_limit))
        task_id = self.n.submit(tool, target(reference))
        self.assertEqual(self.n.process.wait(20), -signal.SIGXFSZ)
        sizes = authority_file_sizes()
        self.assertTrue(all(size < self.copy_limit for size in sizes.values()), sizes)
        stages = list(parent.glob(".stage-*"))
        self.assertEqual(len(stages), 1, sizes)
        partial = stages[0] / incomplete
        self.assertEqual(partial.stat().st_size, self.copy_limit)
        with self.source.open("rb") as source, partial.open("rb") as copied:
            self.assertEqual(hashlib.sha256(source.read(self.copy_limit)).hexdigest(),
                             hashlib.file_digest(copied, "sha256").hexdigest())
        db_url = (self.n.state / "state.db").as_uri() + "?mode=ro"
        with closing(sqlite3.connect(db_url, uri=True)) as database:
            dispatched = database.execute("""SELECT q.phase, o.body->>'lifecycle', o.body->>'outcome_knowledge'
                FROM queue q JOIN current o ON o.kind='Operation' AND o.spec->>'task_id'=q.task
                WHERE q.task=?""", (task_id,)).fetchone()
        self.assertEqual(dispatched, ("Dispatched", "Running", "InFlight"))
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

    def test_restore_checkpoint_failure_reclaims_readonly_stage_without_restart(self):
        nested = self.source.parent / "readonly"
        nested.mkdir(mode=0o700)
        payload = b"retained payload\n"
        (nested / "payload").write_bytes(payload)
        nested.chmod(0o555)
        backup = self.n.perform("backup.create", target(self.volume))
        previous = sorted(self.volumes.iterdir())
        daemon_pid = self.n.process.pid
        with closing(sqlite3.connect(self.n.state / "state.db", timeout=10)) as database:
            # The checkpoint follows content verification, restored modes and sync,
            # and precedes the rename that would make the restored volume final.
            with database:
                database.execute("""CREATE TRIGGER lose_restore_checkpoint BEFORE INSERT ON versions
                    WHEN NEW.kind='RestorePublicationCheckpoint'
                    BEGIN SELECT RAISE(ABORT, 'injected restore checkpoint failure'); END""")
            try:
                task_id = self.n.submit("backup.restore", target(backup))
                failed = self.n.wait(task_id)
            finally:
                with database:
                    database.execute("DROP TRIGGER lose_restore_checkpoint")

        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "Known"))
        condition = failed["conditions"][0]
        self.assertEqual((condition["type"], condition["message"]),
                         ("StorageUnavailable", "state database error"), failed)
        self.assertEqual(failed["result"]["artifact_refs"], [])
        operation = failed["operation_refs"][0]["resource_id"]
        self.assertEqual(self.n.get(f"restore-intent-{operation}")["value"]["backup_ref"], backup)
        with self.assertRaises(Failure) as missing:
            self.n.get(f"restore-publication-{operation}")
        self.assertEqual(missing.exception.code, "NotFound")
        self.assertEqual(sorted(self.volumes.iterdir()), previous)
        self.assertIsNone(self.n.process.poll())
        restored = self.n.perform("backup.restore", target(backup))
        data = self.volumes / restored["resource_id"] / "data" / "readonly"
        self.assertEqual(self.n.process.pid, daemon_pid)
        self.assertEqual(data.stat().st_mode & 0o777, 0o555)
        self.assertEqual((data / "payload").read_bytes(), payload)

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
