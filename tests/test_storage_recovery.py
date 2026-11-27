"""Process loss during real disk copies, before a new resource can be published."""
from contextlib import closing
import hashlib
import json
import os
import resource
import shutil
import signal
import sqlite3
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target


def traced_unlinks(trace):
    partial = {}
    for line in trace.splitlines():
        fields = line.split(maxsplit=1)
        if len(fields) != 2:
            continue
        pid, call = fields
        if call.startswith("unlinkat("):
            if call.endswith(" <unfinished ...>"):
                partial[pid] = call.removesuffix(" <unfinished ...>")
            else:
                yield call
        elif call.startswith("<... unlinkat resumed>") and pid in partial:
            yield partial.pop(pid) + call.removeprefix("<... unlinkat resumed>")


class UnlinkTraceTests(unittest.TestCase):
    def test_split_strace_lines_keep_the_exact_thread_and_target(self):
        lines = [
            '18 unlinkat(10</test/quarantine>, "blob-0", 0 <unfinished ...>',
            '21 unlinkat(17</test/other>, "manifest.json", 0) = 0',
            '18 <... unlinkat resumed>) = 0',
            '18 unlinkat(10</test/quarantine>, "manifest.json", 0 <unfinished ...>',
            '21 <... unlinkat resumed>) = ?',
            '18 <... unlinkat resumed>) = ?',
        ]
        self.assertEqual(list(traced_unlinks("\n".join(lines))), [
            'unlinkat(17</test/other>, "manifest.json", 0) = 0',
            'unlinkat(10</test/quarantine>, "blob-0", 0) = 0',
            'unlinkat(10</test/quarantine>, "manifest.json", 0) = ?',
        ])


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
        evidence = n.call("Artifact.Read", {"reference": fresh})
        self.assertEqual((evidence["operation_ref"], evidence["publication"]), (op, "Published"))
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

    def stop_before_publication_rename(self, tool, source, key_suffix):
        n = self.n
        node = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
        n.stop()
        daemon, trace = n.daemon, n.base / f"{key_suffix}.strace"
        n.daemon = ["strace", "-D", "-f", "-o", str(trace), "-e", "trace=renameat2",
                    "-e", "inject=renameat2:error=EINTR:signal=SIGKILL:when=1", "--", *daemon]
        try:
            n.start()
        finally:
            n.daemon = daemon
        self.assertTrue(os.path.samefile(f"/proc/{n.process.pid}/exe", daemon[0]))
        request = n.prepare(tool, target(source), submission_key=key_suffix)
        task_id = n.call("Task.Submit", request)["task_id"]
        self.assertEqual(n.call("Task.Get", {"id": task_id})["lifecycle"], "Queued")
        node = n.call("Node.Get", {})["reference"]
        try:
            n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Enabled"})
        except (ConnectionError, struct.error):
            # The state change can commit before the interrupted response reaches the client.
            pass
        self.assertEqual(n.process.wait(25), -signal.SIGKILL)
        return request, task_id, trace

    def interrupt_restore_rename(self):
        n = self.n
        request, task_id, trace = self.stop_before_publication_rename(
            "backup.restore", self.backup, "lost-restore-rename")

        db_url = (n.state / "state.db").as_uri() + "?mode=ro"
        with closing(sqlite3.connect(db_url, uri=True)) as db:
            row = db.execute("""SELECT q.operation, q.phase, o.body->>'outcome_knowledge', c.body
                FROM queue q JOIN current o ON o.id=q.operation
                JOIN current c ON c.id='restore-publication-'||q.operation
                AND c.kind='RestorePublicationCheckpoint' WHERE q.task=?""", (task_id,)).fetchone()
            head, = db.execute("SELECT value FROM meta WHERE key='head'").fetchone()
        self.assertIsNotNone(row, "process died before committing the restore checkpoint")
        witness = json.loads((n.witness / "head").read_text())
        self.assertIn(head, (witness["committed"], witness.get("pending")))
        operation, phase, knowledge, body = row
        self.assertEqual((phase, knowledge), ("Dispatched", "InFlight"))
        checkpoint = json.loads(body)
        self.assertEqual(checkpoint["backup_ref"], self.backup)
        stage = self.volumes / checkpoint["placement"]["stage_name"]
        final = self.volumes / checkpoint["receipt"]["id"]
        self.assertEqual(final.name, f"volume-{operation}")
        self.assertFalse(os.path.lexists(final))
        staged = stage.stat()
        self.assertEqual(checkpoint["placement"]["directory"], [str(staged.st_dev), str(staged.st_ino)])
        self.assertEqual(checkpoint["placement"]["parent"], [str(stage.parent.stat().st_dev), str(stage.parent.stat().st_ino)])
        with (stage / "data" / "record").open("rb") as record:
            self.assertEqual(hashlib.file_digest(record, "sha256").hexdigest(), self.digest)
        traced = trace.read_text()
        self.assertTrue(any("renameat2(" in line and f'"{stage.name}"' in line and f'"{final.name}"' in line
                            and "RENAME_NOREPLACE" in line for line in traced.splitlines()), traced)
        return request, task_id, operation, stage, final

    @unittest.skipUnless(os.getenv("PSIOS_TEST_RESTORE_RENAME_FAULT") == "1",
                         "requires strace with syscall and signal injection")
    def test_checkpointed_unpublished_restore_reconciles_and_reclaims_exact_stage(self):
        n = self.n
        request, task_id, operation, stage, final = self.interrupt_restore_rename()
        staged_inode = stage.stat().st_ino
        n.start()
        paused = n.wait(task_id)
        self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
        op, = paused["operation_refs"]
        self.assertEqual(op["resource_id"], operation)
        checkpoint = n.get(f"restore-publication-{operation}")["reference"]
        self.assertEqual(stage.stat().st_ino, staged_inode)
        self.assertFalse(final.exists())

        def rejected(code, method, params, token=None):
            with self.assertRaises(Failure) as failure:
                n.call(method, params, token)
            self.assertEqual(failure.exception.code, code, failure.exception)

        deletion = n.wait(n.submit("backup.delete", target(self.backup)), autoapprove=True)
        self.assertEqual((deletion["lifecycle"], deletion["conditions"][0]["type"]), ("Failed", "Conflict"), deletion)
        rejected("OutcomeUnknown", "Task.Retry", {**target(n.get(task_id)["reference"]), "key": key()})
        reader, _ = n.agent(["task.read"])
        inspector = "volume.inspect_publication"
        grant = n.call("Grant.Issue", {"principal_id": "local-owner", "units": 12, "ttl_seconds": 180,
            "actions": [inspector, "task.submit", "task.read"], "tool_targets": [operation]})
        delegation = grant["delegation_ref"]["resource_id"]
        with self.assertRaises(Failure) as unscoped:
            n.prepare(inspector, target(op))
        self.assertEqual(unscoped.exception.code, "PermissionDenied", unscoped.exception)

        def inspect(publication=None):
            prepared = n.prepare(inspector, target(op), delegation=delegation)
            result = n.wait(n.call("Task.Submit", prepared)["task_id"])
            if publication is None:
                self.assertEqual((result["lifecycle"], result["conditions"][0]["type"]),
                                 ("Failed", "PreconditionFailed"), result)
                self.assertEqual(result["result"]["artifact_refs"], [])
                self.assertEqual(n.wait(task_id), paused)
                return None
            self.assertEqual(result["lifecycle"], "Succeeded", result)
            reference, = result["result"]["artifact_refs"]
            proof = n.call("Artifact.Read", {"reference": reference})
            self.assertEqual((proof["publication"], proof["operation_ref"], proof["checkpoint_ref"]),
                             (publication, op, checkpoint), proof)
            return reference

        observed = inspect("NotApplied")
        rejected("PermissionDenied", "Operation.Reconcile", {**target(op), "evidence_ref": observed}, reader["token"])
        final.mkdir(mode=0o700)
        try:
            inspect()  # Both names exist, even though the stage itself is exact.
        finally:
            final.rmdir()

        parked = n.base / "original-restore-stage"
        stage.rename(parked)
        try:
            inspect()  # Both names are absent.
            final.mkdir(mode=0o700)
            try:
                inspect()  # A final-only replacement is also insufficient.
            finally:
                final.rmdir()
            shutil.copytree(parked, stage)
            try:
                self.assertNotEqual(stage.stat().st_ino, staged_inode)
                inspect()  # Identical bytes in a replacement stage do not prove the original restore.
            finally:
                shutil.rmtree(stage)
        finally:
            parked.rename(stage)

        stage.rename(final)
        try:
            inspect("Published")
            rejected("OutcomeUnknown", "Operation.Reconcile", {**target(op), "evidence_ref": observed})
        finally:
            final.rename(stage)
        self.assertEqual(n.wait(task_id), paused)

        # A competing quarantine name forces the asynchronous deletion to remain pending across restart.
        quarantine = self.volumes / f".restore-discard-{operation}"
        quarantine.mkdir(mode=0o700)
        marker = quarantine / "unrelated-data"
        marker.write_text("preserve this directory")
        observed = inspect("NotApplied")
        result = n.call("Operation.Reconcile", {**target(op), "evidence_ref": observed})
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Failed", "Known"), result)
        settled = n.wait(task_id)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"], settled["conditions"][0]["type"]),
                         ("Failed", "Known", "LeaseLost"), settled)
        receipt, = settled["result"]["artifact_refs"]
        self.assertEqual(n.call("Artifact.Read", {"reference": receipt}), {
            "backup_ref": self.backup, "volume_id": final.name, "publication": "NotApplied",
        })
        rejected("NotFound", "Resource.Get", {"id": final.name})
        replay = n.call("Task.Submit", request)
        self.assertTrue(replay["deduplicated"] and replay["task_id"] == task_id, replay)
        rejected("PreconditionFailed", "Task.Retry", {**target(n.get(task_id)["reference"]), "key": key()})
        self.assertFalse(final.exists())

        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.wait(task_id), settled)
        self.assertEqual(stage.stat().st_ino, staged_inode)
        self.assertEqual(marker.read_text(), "preserve this directory")

        # A separately submitted restore can use the backup even while disposal is blocked.
        restored = n.perform("backup.restore", target(self.backup))
        self.assertNotEqual(restored["resource_id"], final.name)
        with (self.volumes / restored["resource_id"] / "data" / "record").open("rb") as record:
            self.assertEqual(hashlib.file_digest(record, "sha256").hexdigest(), self.digest)
        self.assertEqual(stage.stat().st_ino, staged_inode)
        self.assertEqual(marker.read_text(), "preserve this directory")
        marker.unlink()
        quarantine.rmdir()
        deadline = time.monotonic() + 8
        while True:
            audit = n.call("Audit.List", {"limit": 128})
            removed = [entry for entry in audit if (entry["method"], entry["resource"], entry["decision"])
                       == ("Storage.RestoreStageReclaim", operation, "Removed")]
            if removed:
                self.assertEqual(len(removed), 1)
                break
            self.assertLess(time.monotonic(), deadline, "restore stage disposal did not settle")
            time.sleep(0.05)
        self.assertFalse(stage.exists())
        self.assertFalse(quarantine.exists())
        self.assertFalse(final.exists())
        n.perform("backup.delete", target(self.backup))

    @unittest.skipUnless(os.getenv("PSIOS_TEST_RESTORE_RENAME_FAULT") == "1",
                         "requires strace with syscall and signal injection")
    def test_checkpointed_unpublished_backup_reconciles_and_reclaims_exact_stage(self):
        n = self.n
        request, task_id, trace = self.stop_before_publication_rename(
            "backup.create", self.volume, "lost-backup-publication")
        with closing(sqlite3.connect(n.state / "state.db")) as database:
            retained = database.execute(
                "SELECT q.operation,q.phase,o.body->>'outcome_knowledge',c.body "
                "FROM queue q JOIN current o ON o.id=q.operation JOIN current c "
                "ON c.id='backup-publication-'||q.operation WHERE q.task=?", (task_id,),
            ).fetchone()
            self.assertIsNotNone(retained, "process died before the backup checkpoint committed")
            operation, phase, knowledge, body = retained
            self.assertEqual((phase, knowledge), ("Dispatched", "InFlight"))
            checkpoint = json.loads(body)
            self.assertEqual(checkpoint["source_ref"], self.volume)
            index = database.execute(
                "SELECT * FROM backup_stage_custody WHERE operation=?", (operation,),
            ).fetchone()
            self.assertIsNotNone(index)
            stage = self.backups / checkpoint["placement"]["stage_name"]
            final = self.backups / checkpoint["receipt"]["id"]
            self.assertEqual(final.name, f"backup-{operation}")
            self.assertEqual(index[:2], (operation, stage.name))
            self.assertIsNone(index[-1])
            head, = database.execute("SELECT value FROM meta WHERE key='head'").fetchone()
        witness = json.loads((n.witness / "head").read_text())
        self.assertEqual(witness, {"committed": head})
        inode = stage.stat().st_ino
        self.assertEqual(checkpoint["placement"]["directory"], [stage.stat().st_dev, inode])
        self.assertEqual(checkpoint["placement"]["parent"], [self.backups.stat().st_dev, self.backups.stat().st_ino])
        self.assertFalse(os.path.lexists(final))
        with (stage / "blob-0").open("rb") as record:
            self.assertEqual(hashlib.file_digest(record, "sha256").hexdigest(), self.digest)
        self.assertTrue(any("renameat2(" in line and f'"{stage.name}"' in line and f'"{final.name}"' in line
                            and "RENAME_NOREPLACE" in line for line in trace.read_text().splitlines()))

        with closing(sqlite3.connect(n.state / "state.db")) as database, database:
            database.execute("DELETE FROM backup_stage_custody WHERE operation=?", (operation,))
        try:
            for command in ("verify", "serve"):
                corrupt = subprocess.run(n.daemon + [command], capture_output=True, text=True, timeout=15)
                self.assertNotEqual(corrupt.returncode, 0, corrupt)
                self.assertIn("active backup checkpoint is missing its exact stage custody", corrupt.stderr)
            self.assertEqual(stage.stat().st_ino, inode, "startup reclaimed unresolved evidence")
        finally:
            with closing(sqlite3.connect(n.state / "state.db")) as database, database:
                database.execute("INSERT INTO backup_stage_custody VALUES(?,?,?,?)", index)
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)
        n.start()
        paused = n.wait(task_id)
        self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
        op, = paused["operation_refs"]
        checkpoint_ref = n.get(f"backup-publication-{operation}")["reference"]
        self.assertEqual(stage.stat().st_ino, inode)
        self.assertEqual(op["resource_id"], operation)

        blocked = n.wait(n.submit("backup.create", target(self.volume)))
        self.assertEqual((blocked["lifecycle"], blocked["conditions"][0]["type"]), ("Failed", "Conflict"), blocked)
        inspector = "backup.inspect_publication"
        with self.assertRaises(Failure) as refused:
            n.prepare(inspector, target(op))
        self.assertEqual(refused.exception.code, "PermissionDenied")
        grant = n.call("Grant.Issue", {"principal_id": "local-owner", "units": 12, "ttl_seconds": 180,
            "actions": [inspector, "task.submit", "task.read"], "tool_targets": [operation]})
        delegation = grant["delegation_ref"]["resource_id"]

        def inspect(publication=None):
            prepared = n.prepare(inspector, target(op), delegation=delegation)
            result = n.wait(n.call("Task.Submit", prepared)["task_id"])
            if publication is None:
                self.assertEqual((result["lifecycle"], result["conditions"][0]["type"]),
                                 ("Failed", "PreconditionFailed"), result)
                self.assertEqual(result["result"]["artifact_refs"], [])
                self.assertEqual(n.wait(task_id), paused)
                return None
            self.assertEqual(result["lifecycle"], "Succeeded", result)
            reference, = result["result"]["artifact_refs"]
            proof = n.call("Artifact.Read", {"reference": reference})
            self.assertEqual((proof["publication"], proof["operation_ref"], proof["checkpoint_ref"]),
                             (publication, op, checkpoint_ref), proof)
            return reference

        evidence = inspect("NotApplied")
        reader, _ = n.agent(["task.read"])
        with self.assertRaises(Failure) as denied:
            n.call("Operation.Reconcile", {**target(op), "evidence_ref": evidence}, reader["token"])
        self.assertEqual(denied.exception.code, "PermissionDenied")
        final.mkdir(mode=0o700)
        try:
            inspect()  # Both paths exist.
        finally:
            final.rmdir()
        parked = n.base / "parked-backup-stage"
        stage.rename(parked)
        try:
            inspect()  # Neither path exists.
            shutil.copytree(parked, stage)
            try:
                self.assertNotEqual(stage.stat().st_ino, inode)
                inspect()  # Exact contents alone cannot identify the directory.
            finally:
                shutil.rmtree(stage)
        finally:
            parked.rename(stage)
        stage.rename(final)
        try:
            inspect("Published")
            with self.assertRaises(Failure) as stale:
                n.call("Operation.Reconcile", {**target(op), "evidence_ref": evidence})
            self.assertEqual(stale.exception.code, "OutcomeUnknown")
        finally:
            final.rename(stage)

        quarantine = self.backups / f".backup-discard-{operation}"
        quarantine.mkdir(mode=0o700)
        marker = quarantine / "unrelated"
        marker.write_text("keep original contents")
        evidence = inspect("NotApplied")
        settled_op = n.call("Operation.Reconcile", {**target(op), "evidence_ref": evidence})
        self.assertEqual((settled_op["lifecycle"], settled_op["outcome_knowledge"]), ("Failed", "Known"))
        settled = n.wait(task_id)
        self.assertEqual(settled["conditions"][0]["type"], "LeaseLost")
        receipt, = settled["result"]["artifact_refs"]
        self.assertEqual(n.call("Artifact.Read", {"reference": receipt}), {
            "source_volume_ref": self.volume, "backup_id": final.name, "publication": "NotApplied",
        })
        with self.assertRaises(Failure) as missing:
            n.get(final.name)
        self.assertEqual(missing.exception.code, "NotFound")
        replay = n.call("Task.Submit", request)
        self.assertTrue(replay["deduplicated"] and replay["task_id"] == task_id, replay)
        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)
        n.start()
        successor = n.perform("backup.create", target(self.volume))
        self.assertNotEqual(successor["resource_id"], final.name)
        self.assertEqual(stage.stat().st_ino, inode)
        self.assertEqual(marker.read_text(), "keep original contents")
        n.stop()
        daemon, trace = n.daemon, n.base / "backup-cleanup-cut.strace"
        manifest = quarantine / "manifest.json"
        n.daemon = ["strace", "-D", "-f", "-yy", "-o", str(trace),
                    "-e", "trace=unlinkat", "-e", "inject=unlinkat:error=EINTR:signal=SIGKILL:when=2",
                    "--", *daemon]
        try:
            n.start()
        finally:
            n.daemon = daemon
        self.assertTrue(os.path.samefile(f"/proc/{n.process.pid}/exe", daemon[0]))
        marker.unlink()
        quarantine.rmdir()
        self.assertEqual(n.process.wait(25), -signal.SIGKILL)
        traced = trace.read_text()
        calls = list(traced_unlinks(traced))
        self.assertTrue(any(quarantine.name in call and '"blob-0"' in call and "= 0" in call
                            for call in calls), traced)
        self.assertTrue(any(quarantine.name in call and '"manifest.json"' in call
                            and ("= ?" in call or "INJECTED" in call) for call in calls), traced)
        self.assertFalse(stage.exists())
        self.assertEqual(quarantine.stat().st_ino, inode)
        self.assertTrue(manifest.is_file())
        replacement = quarantine / "blob-0"
        self.assertFalse(replacement.exists(), "cleanup was interrupted before removing its first member")
        replacement.write_bytes(b"unrelated file with a valid backup member name")
        log = n.base / "backup-cleanup-resume.stderr"
        n.daemon = ["/bin/sh", "-c", 'log=$1; shift; exec "$@" 2>"$log"',
                    "capture-backup-cleanup", str(log), *daemon]
        try:
            n.start()
        finally:
            n.daemon = daemon
        self.assertEqual(n.wait(task_id), settled)
        deadline = time.monotonic() + 8
        while "psiosd backup stage reclamation: PreconditionFailed" not in log.read_text():
            self.assertLess(time.monotonic(), deadline, "the worker did not observe the foreign member")
            time.sleep(0.05)
        self.assertEqual(replacement.read_bytes(), b"unrelated file with a valid backup member name")
        self.assertTrue(manifest.is_file())
        replacement.unlink()
        deadline = time.monotonic() + 8
        while True:
            observations = [entry for entry in n.call("Audit.List", {"limit": 128})
                            if (entry["method"], entry["resource"]) == ("Storage.BackupStageReclaim", operation)]
            if observations:
                self.assertEqual([entry["decision"] for entry in observations], ["Removed"])
                break
            self.assertLess(time.monotonic(), deadline, "backup stage reclamation did not settle")
            time.sleep(0.05)
        self.assertEqual(n.wait(task_id), settled)
        self.assertFalse(stage.exists())
        self.assertFalse(quarantine.exists())
        self.assertFalse(final.exists())
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)


if __name__ == "__main__":
    unittest.main()
