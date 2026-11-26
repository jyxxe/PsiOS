"""Confirm filesystem deletion across lost receipts and daemon restarts."""
from contextlib import closing
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, target


FUTURE_DUE = "2099-01-01T00:00:00.000Z"


def defer_reclamation(node):
    with closing(sqlite3.connect(node.state / "state.db", timeout=10)) as database:
        database.executescript(f"""CREATE TRIGGER defer_fixture_reclamation AFTER INSERT ON storage_reclamations
            BEGIN UPDATE storage_reclamations SET ready_after='{FUTURE_DUE}' WHERE retirement=NEW.retirement; END""")


def release_reclamation(node, retirement):
    with closing(sqlite3.connect(node.state / "state.db")) as database, database:
        updated = database.execute("UPDATE storage_reclamations SET ready_after=? WHERE retirement=?",
                                   ("2000-01-01T00:00:00.000Z", retirement)).rowcount
        assert updated == 1, retirement


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

    def test_missing_and_stale_reclamation_rows_refuse_startup_without_touching_retired_bytes(self):
        n = self.n
        defer_reclamation(n)
        original_inode = self.original.stat().st_ino
        task_id = n.submit("volume.delete", target(self.volume))
        task = n.wait(task_id, autoapprove=True)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"))
        operation = self.operation(task_id)
        retired = self.original.parent / f".retired-{operation['resource_id']}"
        pending = f"storage-retirement-{operation['resource_id']}"
        self.assertEqual(n.get(pending)["value"]["lifecycle"], "PendingCleanup")
        self.assertEqual(retired.stat().st_ino, original_inode)
        self.assertFalse(self.original.exists())
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as database:
            database.execute("DROP TRIGGER defer_fixture_reclamation")

        independent = n.perform("volume.create", {"name": "independent-retirement-custody"})
        other = n.submit("volume.delete", target(independent))
        self.assertEqual(n.wait(other, autoapprove=True)["lifecycle"], "Succeeded")
        reclaimed = f"storage-retirement-{self.operation(other)['resource_id']}"
        self.wait_for(lambda: n.get(reclaimed)["value"]["lifecycle"] == "Reclaimed", "fixture retirement did not reclaim")
        retained = n.perform("volume.create", {"name": "unrelated-retirement-custody"})
        retained_data = n.state / "volumes" / retained["resource_id"] / "data" / "payload"
        retained_data.write_bytes(b"unrelated business data")
        n.stop(signal.SIGKILL)

        def check_copy(name, mutate, message):
            base = n.base / name
            base.mkdir(mode=0o700)
            state = base / "state"
            witness = base / "witness"
            shutil.copytree(n.state, state)
            shutil.copytree(n.witness, witness)
            configuration = json.loads(n.config.read_text())
            configuration.update(state_dir=str(state), witness_dir=str(witness), socket=str(base / "api.sock"))
            config = base / "config.json"
            config.write_text(json.dumps(configuration))
            with closing(sqlite3.connect(state / "state.db")) as database, database:
                mutate(database)
            original_data = state / retired.relative_to(n.state) / "data" / "business-data"
            before = original_data.stat()
            for command in ("verify", "serve"):
                result = subprocess.run([n.daemon[0], "--config", str(config), command],
                                        capture_output=True, text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("RecoveryRequired", result.stderr)
                self.assertIn(message, result.stderr)
                self.assertFalse((base / "api.sock").exists())
                self.assertEqual(original_data.read_bytes(), b"persistent record")
                self.assertEqual(original_data.stat().st_ino, before.st_ino)

        check_copy("missing-row", lambda db: db.execute(
            "DELETE FROM storage_reclamations WHERE retirement=?", (pending,)),
            "pending storage retirement is missing its reclamation row")
        check_copy("stale-row", lambda db: db.execute(
            "INSERT INTO storage_reclamations VALUES(?,?)", (reclaimed, FUTURE_DUE)),
            "storage reclamation row does not name a pending retirement")
        check_copy("invalid-due", lambda db: db.execute(
            "UPDATE storage_reclamations SET ready_after='invalid-time' WHERE retirement=?", (pending,)),
            "storage reclamation row has an invalid due time")
        check_copy("wrong-location", lambda db: db.execute(
            "UPDATE versions SET body=json_set(body,'$.location.directory.inode',?) "
            "WHERE id=? AND body->>'lifecycle'='PendingCleanup'", (str(original_inode + 1), pending)),
            "storage reclamation is not bound to its committed deletion")

        subprocess.run([*n.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)
        n.start()
        self.assertEqual(n.get(pending)["value"]["lifecycle"], "PendingCleanup")
        self.assertEqual((retired / "data" / "business-data").read_bytes(), b"persistent record")
        self.assertEqual(retired.stat().st_ino, original_inode)
        with closing(sqlite3.connect(n.state / "state.db")) as database:
            self.assertEqual(database.execute("SELECT ready_after FROM storage_reclamations WHERE retirement=?",
                                              (pending,)).fetchone(), (FUTURE_DUE,))
        n.stop()
        # Release only the fixture's due-time gate. The original approval,
        # committed ownership and physical directory remain unchanged.
        release_reclamation(n, pending)
        n.start()
        self.wait_for(lambda: n.get(pending)["value"]["lifecycle"] == "Reclaimed", "original retirement did not reclaim")
        self.assertFalse(retired.exists())
        self.assertEqual(retained_data.read_bytes(), b"unrelated business data")
        self.assertEqual(self.operation(task_id), operation)
        n.stop()
        subprocess.run([*n.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_STORAGE_RETIREMENT_PRIOR_BIN_DIR"), "requires genuine format 39 binaries")
    def test_format_39_pending_cleanup_keeps_its_original_approval_and_due_time(self):
        node = Node(os.environ["PSIOS_TEST_STORAGE_RETIREMENT_PRIOR_BIN_DIR"])
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "legacy-storage-retirement"})
        volumes = node.state / "volumes"
        source = volumes / volume["resource_id"]
        (source / "data" / "payload").write_bytes(b"original format 39 bytes")
        inode = source.stat().st_ino
        defer_reclamation(node)
        task = node.submit("volume.delete", target(volume))
        settled = node.wait(task, autoapprove=True)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"))
        [op] = settled["operation_refs"]
        retired = volumes / f".retired-{op['resource_id']}"
        pending = f"storage-retirement-{op['resource_id']}"
        self.assertEqual(retired.stat().st_ino, inode)
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(self.n.binaries / "psiosd")
        old = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertEqual((old["database_format"], old["migration_required"]), (39, True))
        node.start()
        self.assertEqual(node.call("Task.Get", {"id": task}), settled)
        self.assertEqual(node.get(pending)["value"]["lifecycle"], "PendingCleanup")
        self.assertEqual(retired.stat().st_ino, inode)
        self.assertEqual((retired / "data" / "payload").read_bytes(), b"original format 39 bytes")
        with closing(sqlite3.connect(node.state / "state.db")) as database:
            self.assertEqual(database.execute("SELECT ready_after FROM storage_reclamations WHERE retirement=?",
                                              (pending,)).fetchone(), (FUTURE_DUE,))
        node.stop()
        release_reclamation(node, pending)
        node.start()
        self.wait_for(lambda: node.get(pending)["value"]["lifecycle"] == "Reclaimed", "legacy retirement did not reclaim")
        self.assertEqual(node.call("Task.Get", {"id": task}), settled)
        self.assertFalse(retired.exists())


if __name__ == "__main__":
    unittest.main()
