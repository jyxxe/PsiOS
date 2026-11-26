"""Real local entry retirement, unresolved placement and resumed safe disposal."""

from contextlib import closing, contextmanager
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target


TOOL = "volume.remove_entry"
KIND = "VolumeEntryRetirement"


class VolumeEntryRetirementTests(unittest.TestCase):
    def setUp(self):
        self.binaries = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")
        self.n = Node(self.binaries)
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "entry-retirement"})
        self.volume = self.n.perform("volume.mkdir", {
            "target": target(self.volume), "path": "nested",
        })["volume_ref"]
        self.n.perform("volume.write", {"id": self.volume["resource_id"],
                                        "expected_resource_version": self.volume["version"],
                                        "name": "other", "content": "other survives"})
        self.volume = self.n.get(self.volume["resource_id"])["reference"]
        self.wrapper = self.n.state / "volumes" / self.volume["resource_id"]
        self.source = self.wrapper / "data" / "nested" / "record"
        self.source.write_bytes(b"approved original")
        self.database = self.n.state / "state.db"

    @contextmanager
    def reject_receipt(self, name, predicate, database=None):
        database = database or self.database
        with closing(sqlite3.connect(database, timeout=8)) as db, db:
            db.execute(f"""CREATE TRIGGER {name} BEFORE INSERT ON versions
                WHEN {predicate}
                BEGIN SELECT RAISE(ABORT, 'injected entry retirement receipt loss'); END""")
        try:
            yield
        finally:
            with closing(sqlite3.connect(database, timeout=8)) as db, db:
                db.execute(f"DROP TRIGGER {name}")

    def submit(self, path="nested/record", volume=None):
        return self.n.submit(TOOL, {"target": target(volume or self.volume), "path": path})

    def assert_error(self, code, method, params, token=None):
        with self.assertRaises(Failure) as error:
            self.n.call(method, params, token)
        self.assertEqual(error.exception.code, code, error.exception)

    def state(self, operation):
        with closing(sqlite3.connect(self.database)) as db:
            return db.execute("SELECT body->>'lifecycle' FROM current WHERE kind=? AND spec->>'operation_id'=?",
                              (KIND, operation)).fetchone()

    def wait_until(self, condition, message="retirement condition did not become true"):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            if condition():
                return
            time.sleep(0.05)
        self.fail(message)

    def quarantine(self, operation):
        return self.wrapper / ".entry-retirements" / f"entry-{operation}"

    def assert_volume_fenced(self, original, op):
        result = self.n.wait(self.n.submit("volume.read", dict(target(self.volume), name="other")))
        self.assertEqual((result["lifecycle"], result["conditions"][0]["type"]), ("Failed", "Conflict"), result)
        self.assert_error("OutcomeUnknown", "Task.Retry", {
            **target(self.n.get(original)["reference"]), "key": key(),
        })
        self.assertEqual(self.n.call("Task.Get", {"id": original})["operation_refs"], [op])

    def check_missing_index_blocks_startup(self, operation, quarantine, inode):
        with closing(sqlite3.connect(self.database)) as db, db:
            reservation = db.execute("SELECT * FROM volume_entry_retirements WHERE operation=?", (operation,)).fetchone()
            self.assertIsNotNone(reservation)
            db.execute("DELETE FROM volume_entry_retirements WHERE operation=?", (operation,))
        try:
            for command in ("verify", "serve"):
                result = subprocess.run(self.n.daemon + [command], capture_output=True, text=True, timeout=15)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("RecoveryRequired", result.stdout + result.stderr)
                self.assertIn("custody index disagrees", result.stdout + result.stderr)
            self.assertEqual(quarantine.stat().st_ino, inode)
            self.assertEqual(quarantine.read_bytes(), b"approved original")
        finally:
            with closing(sqlite3.connect(self.database)) as db, db:
                db.execute("INSERT INTO volume_entry_retirements VALUES(?,?,?,?,?)", reservation)

    def test_exact_placement_recovers_after_restart_and_resumes_cleanup_receipt(self):
        n = self.n
        original_inode = self.source.stat().st_ino
        with self.reject_receipt("lose_entry_catalog", f"NEW.id='{self.volume['resource_id']}' AND NEW.kind='Volume'"):
            task = self.submit()
            paused = n.wait(task, autoapprove=True)
            self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
            op, = paused["operation_refs"]
            quarantine = self.quarantine(op["resource_id"])
            self.assertFalse(self.source.exists())
            self.assertEqual(quarantine.stat().st_ino, original_inode)
            self.assertEqual(quarantine.read_bytes(), b"approved original")
            self.assertEqual(self.state(op["resource_id"]), ("Prepared",))
            n.stop(signal.SIGKILL)
        self.check_missing_index_blocks_startup(op["resource_id"], quarantine, original_inode)
        n.start()
        self.assert_volume_fenced(task, op)

        outsider, _ = n.agent(["task.read"])
        self.assert_error("PermissionDenied", "Operation.Reconcile", target(op), outsider["token"])

        saved = quarantine.rename(n.base / "parked-original")
        self.assert_error("OutcomeUnknown", "Operation.Reconcile", target(op))
        quarantine.write_bytes(b"foreign inode")
        self.assert_error("OutcomeUnknown", "Operation.Reconcile", target(op))
        self.assertEqual(quarantine.read_bytes(), b"foreign inode")
        quarantine.unlink()
        saved.rename(quarantine)
        self.source.write_bytes(b"conflicting original pathname")
        self.assert_error("OutcomeUnknown", "Operation.Reconcile", target(op))
        self.assertEqual(self.source.read_bytes(), b"conflicting original pathname")
        self.assertEqual(quarantine.stat().st_ino, original_inode)
        self.source.unlink()
        self.assert_volume_fenced(task, op)

        with self.reject_receipt("lose_entry_disposal", f"NEW.id='volume-entry-retirement-{op['resource_id']}'"
                                 " AND NEW.body->>'lifecycle'='Reclaimed'"):
            fixed = n.call("Operation.Reconcile", target(op))
            self.assertEqual(fixed["outcome_knowledge"], "Known")
            done = n.wait(task)
            self.assertEqual(done["lifecycle"], "Succeeded")
            artifact, = done["result"]["artifact_refs"]
            receipt = n.call("Artifact.Read", {"reference": artifact})
            self.assertEqual(receipt["receipt"], {"path": "nested/record"})
            self.volume = receipt["volume_ref"]
            self.wait_until(lambda: not quarantine.exists())
            self.assertEqual(self.state(op["resource_id"]), ("PendingCleanup",))
            still_held = n.wait(n.submit("volume.delete", target(self.volume)), autoapprove=True)
            self.assertEqual((still_held["lifecycle"], still_held["conditions"][0]["type"]),
                             ("Failed", "Conflict"), still_held)
            self.source.write_bytes(b"new entry after settlement")
            n.stop(signal.SIGKILL)
        n.start()
        self.wait_until(lambda: self.state(op["resource_id"]) == ("Reclaimed",))
        self.assertEqual(self.source.read_bytes(), b"new entry after settlement")
        self.assertFalse(quarantine.exists())
        self.assertEqual(n.perform("volume.read", dict(target(self.volume), name="other"))["content"], "other survives")
        unknown = quarantine.parent / "unaccounted-entry"
        unknown.write_bytes(b"unaccounted entry survives whole-volume deletion")
        blocked = n.wait(n.submit("volume.delete", target(self.volume)), autoapprove=True)
        self.assertEqual((blocked["lifecycle"], blocked["conditions"][0]["type"]), ("Failed", "Conflict"), blocked)
        self.assertEqual(unknown.read_bytes(), b"unaccounted entry survives whole-volume deletion")
        self.assertEqual(self.source.read_bytes(), b"new entry after settlement")
        unknown.unlink()
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)

    def test_keeps_nonempty_hardlinked_and_symlink_entries(self):
        n = self.n
        regular = self.source
        extra = self.wrapper / "data" / "linked"
        os.link(regular, extra)
        linked = n.wait(self.submit(), autoapprove=True)
        self.assertEqual((linked["lifecycle"], linked["outcome_knowledge"], linked["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "UnsupportedCapability"), linked)
        self.assertEqual(regular.stat().st_ino, extra.stat().st_ino)
        extra.unlink()

        extra.symlink_to(regular)
        link_inode = extra.lstat().st_ino
        link_task = n.wait(self.submit("linked"), autoapprove=True)
        self.assertEqual((link_task["lifecycle"], link_task["outcome_knowledge"], link_task["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "UnsupportedCapability"), link_task)
        self.assertTrue(extra.is_symlink())
        self.assertEqual(extra.lstat().st_ino, link_inode)

        nonempty = n.wait(self.submit("nested"), autoapprove=True)
        self.assertEqual((nonempty["lifecycle"], nonempty["outcome_knowledge"], nonempty["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PreconditionFailed"), nonempty)
        self.assertEqual(regular.read_bytes(), b"approved original")

        self.volume = n.perform(TOOL, {"target":target(self.volume), "path":"nested/record"})["volume_ref"]
        self.volume = n.perform(TOOL, {"target":target(self.volume), "path":"nested"})["volume_ref"]
        self.assertFalse(regular.parent.exists())
        self.assertTrue(extra.is_symlink())
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)

    def test_failed_entry_reclamation_receipt_does_not_strand_another_volume(self):
        n = self.n
        other = n.perform("volume.create", {"name": "independent-entry-retirement"})
        other = n.perform("volume.mkdir", {"target": target(other), "path": "empty"})["volume_ref"]
        other_wrapper = n.state / "volumes" / other["resource_id"]
        other_path = other_wrapper / "data" / "empty"
        self.assertTrue(other_path.is_dir())
        first = self.submit()
        waiting = n.wait(first)
        self.assertEqual(waiting["lifecycle"], "WaitingApproval")
        first_id = waiting["operation_refs"][0]["resource_id"]
        first_path = self.quarantine(first_id)
        rejection = f"NEW.id='volume-entry-retirement-{first_id}' AND NEW.kind='{KIND}' AND NEW.body->>'lifecycle'='Reclaimed'"

        with self.source.open("rb") as original, self.reject_receipt("lose_first_entry_reclaim", rejection):
            first_inode = os.fstat(original.fileno())
            done = n.wait(first, autoapprove=True)
            self.assertEqual((done["lifecycle"], done["outcome_knowledge"]), ("Succeeded", "Known"), done)
            first_operation, = done["operation_refs"]
            self.assertEqual(first_operation["resource_id"], first_id)
            self.wait_until(lambda: not first_path.exists(), "the first quarantined file was never physically removed")
            self.assertFalse(self.source.exists())
            self.assertEqual(os.fstat(original.fileno()).st_nlink, 0)
            self.assertEqual(self.state(first_id), ("PendingCleanup",))
            self.source.write_bytes(b"distinct new file after retirement")
            replacement = self.source.stat()
            self.assertNotEqual((replacement.st_dev, replacement.st_ino), (first_inode.st_dev, first_inode.st_ino))

            n.stop(signal.SIGKILL)
            n.start()
            self.assertEqual(self.state(first_id), ("PendingCleanup",))
            second = n.wait(self.submit("empty", other), autoapprove=True)
            self.assertEqual((second["lifecycle"], second["outcome_knowledge"]), ("Succeeded", "Known"), second)
            second_operation, = second["operation_refs"]
            second_id = second_operation["resource_id"]
            second_path = other_wrapper / ".entry-retirements" / f"entry-{second_id}"
            self.wait_until(lambda: self.state(second_id) == ("Reclaimed",),
                            "the failed entry receipt prevented another volume's reclamation")
            self.assertFalse(other_path.exists())
            self.assertFalse(second_path.exists())
            self.assertFalse(first_path.exists())
            self.assertEqual(self.state(first_id), ("PendingCleanup",))
            with closing(sqlite3.connect(self.database, timeout=8)) as db:
                self.assertEqual(db.execute("SELECT operation,phase FROM volume_entry_retirements").fetchall(),
                                 [(first_id, "PendingCleanup")])
                self.assertEqual(db.execute("SELECT entries,bytes FROM volume_entry_retirement_budget").fetchone(),
                                 (1, len(b"approved original")))
            self.assertEqual(original.read(), b"approved original")

        self.wait_until(lambda: self.state(first_id) == ("Reclaimed",), "the original entry receipt did not resume")
        with closing(sqlite3.connect(self.database, timeout=8)) as db:
            self.assertEqual(db.execute("SELECT entries,bytes FROM volume_entry_retirement_budget").fetchone(), (0, 0))
            for operation, decision in ((first_id, "ConfirmedAbsent"), (second_id, "Removed")):
                rows = db.execute("SELECT decision FROM audit WHERE method='Storage.VolumeEntryReclaim' AND resource=?",
                                  (operation,)).fetchall()
                self.assertEqual(rows, [(decision,)])
        current = self.source.stat()
        self.assertEqual((current.st_dev, current.st_ino), (replacement.st_dev, replacement.st_ino))
        self.assertEqual(self.source.read_bytes(), b"distinct new file after retirement")
        self.assertEqual(n.wait(first)["operation_refs"], [first_operation])
        self.assertEqual(n.wait(second["task_id"])["operation_refs"], [second_operation])
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)

    @unittest.skipUnless(os.getenv("PSIOS_ENTRY_RETIREMENT_PRIOR_BIN_DIR"), "requires genuine format 112 binaries")
    def test_legacy_unlink_without_evidence_remains_unknown_after_migration(self):
        node = Node(os.environ["PSIOS_ENTRY_RETIREMENT_PRIOR_BIN_DIR"])
        self.addCleanup(node.close)
        prior = node.daemon.copy()
        volume = node.perform("volume.create", {"name": "legacy-entry-removal"})
        source = node.state / "volumes" / volume["resource_id"] / "data" / "legacy"
        source.write_bytes(b"original legacy entry")
        with self.reject_receipt("lose_legacy_unlink_receipt", f"NEW.id='{volume['resource_id']}' AND NEW.kind='Volume'",
                                 node.state / "state.db"):
            task = node.submit(TOOL, {"target": target(volume), "path": "legacy"})
            old_result = node.wait(task, autoapprove=True)
            self.assertEqual((old_result["lifecycle"], old_result["outcome_knowledge"]), ("Paused", "Unknown"))
            op, = old_result["operation_refs"]
            self.assertFalse(source.exists())
            node.stop(signal.SIGKILL)

        node.daemon[0] = self.n.daemon[0]
        rehearsal = subprocess.run(node.daemon + ["verify", "--rehearse-migration"],
                                   check=True, capture_output=True, text=True, timeout=20)
        self.assertEqual(json.loads(rehearsal.stdout)["database_format"], 112)
        node.start()
        with self.assertRaises(Failure) as unknown:
            node.call("Operation.Reconcile", target(op))
        self.assertEqual(unknown.exception.code, "UnsupportedCapability")
        self.assertEqual(node.wait(task), old_result)
        node.stop()
        rejected = subprocess.run(prior + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("SchemaVersionUnsupported", rejected.stdout + rejected.stderr)
        subprocess.run(node.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_ENTRY_RETIREMENT_FAULT") == "1" and shutil.which("strace"),
                         "requires real strace renameat2 fault injection")
    def test_rename_failure_reconciles_exact_original_without_replaying(self):
        n = self.n
        original_stat = self.source.stat()
        n.stop()
        daemon = n.daemon
        trace = n.base / "rename.log"
        n.daemon = ["strace", "-D", "-f", "-e", "trace=renameat2",
                    "-e", "inject=renameat2:error=EIO:when=1", "-o", str(trace), *daemon]
        try:
            n.start()
            task = self.submit()
            paused = n.wait(task, autoapprove=True)
            self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
            op, = paused["operation_refs"]
            evidence = trace.read_text()
            self.assertIn("renameat2(", evidence)
            self.assertIn("EIO (Input/output error) (INJECTED)", evidence)
            self.assertEqual(self.source.stat().st_ino, original_stat.st_ino)
            self.assertFalse(self.quarantine(op["resource_id"]).exists())
        finally:
            n.stop()
            n.daemon = daemon
        n.start()
        result = n.call("Operation.Reconcile", target(op))
        self.assertEqual(result["outcome_knowledge"], "Known")
        final = n.wait(task)
        self.assertEqual((final["lifecycle"], final["conditions"][0]["type"]), ("Failed", "LeaseLost"))
        self.assertEqual(self.source.stat().st_ino, original_stat.st_ino)
        self.assertEqual(self.source.read_bytes(), b"approved original")
        self.assertEqual(self.state(op["resource_id"]), ("NotApplied",))
        n.perform(TOOL, {"target":target(self.volume), "path":"nested/record"})
        self.assertFalse(self.source.exists())
