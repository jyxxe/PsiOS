"""Reconcile a real nested mkdir without replaying it or adopting a substitute."""

from contextlib import closing, contextmanager
from datetime import datetime, timezone
import errno
import json
import os
import shutil
import signal
import sqlite3
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target


TOOL = "volume.mkdir"
KIND = "VolumeDirectoryCreation"
PRIOR_BIN = os.getenv("PSIOS_VOLUME_MKDIR_PRIOR_BIN_DIR")


def identity(path):
    metadata = path.stat()
    return metadata.st_dev, metadata.st_ino


def private_acl():
    # Linux's POSIX ACL xattr includes at least one distinct named subject.
    current = os.getuid()
    named = current + 1 if current < 65534 else current - 1
    return struct.pack("<I", 2) + b"".join(struct.pack("<HHI", tag, mode, actor) for tag, mode, actor in (
        (1, 7, 0xFFFFFFFF), (2, 0, named), (4, 0, 0xFFFFFFFF),
        (16, 0, 0xFFFFFFFF), (32, 0, 0xFFFFFFFF),
    ))


@contextmanager
def lose_volume_receipt(database, volume):
    with closing(sqlite3.connect(database, timeout=10)) as db, db:
        db.execute(f"""CREATE TRIGGER lose_mkdir_volume_revision BEFORE INSERT ON versions
            WHEN NEW.kind='Volume' AND NEW.id='{volume['resource_id']}'
            BEGIN SELECT RAISE(ABORT,'fixture withheld the mkdir catalogue receipt'); END""")
    try:
        yield
    finally:
        with closing(sqlite3.connect(database, timeout=10)) as db, db:
            db.execute("DROP TRIGGER lose_mkdir_volume_revision")


class VolumeMkdirRecovery(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        volume = self.n.perform("volume.create", {"name": "directory-custody"})
        self.volume = self.n.perform("volume.write", {**target(volume), "name": "keep",
                                                     "content": "Outside the target parent.\n"})["volume_ref"]
        self.wrapper = self.n.state / "volumes" / self.volume["resource_id"]
        self.root = self.wrapper / "data"
        self.parent = self.root / "parent"
        self.parent.mkdir(mode=0o700)
        self.sibling = self.parent / "neighbour"
        self.sibling.write_bytes(b"Beside the target directory.\n")
        self.sentinel = self.root / "keep"
        self.protected = {path: (identity(path), path.read_bytes()) for path in (self.sentinel, self.sibling)}
        self.leaf = self.parent / "created"
        self.path = self.leaf.relative_to(self.root).as_posix()
        self.database = self.n.state / "state.db"

    def prepare(self):
        return self.n.prepare(TOOL, {"target": target(self.volume), "path": self.path})

    def assert_protected(self):
        for path, (exact, content) in self.protected.items():
            self.assertEqual(identity(path), exact)
            self.assertEqual(path.read_bytes(), content)

    def deny(self, code, method, params, token=None):
        with self.assertRaises(Failure) as denied:
            self.n.call(method, params, token)
        self.assertEqual(denied.exception.code, code, denied.exception)

    def paused(self, task_id):
        task = self.n.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Paused", "Unknown"), task)
        operation, = task["operation_refs"]
        return task, operation

    def stage(self, operation, *, published):
        identifier = operation["resource_id"]
        with closing(sqlite3.connect(self.database.as_uri() + "?mode=ro", uri=True, timeout=10)) as db:
            record = db.execute("SELECT kind,spec,body FROM current WHERE id=?",
                                (f"volume-directory-creation-{identifier}",)).fetchone()
        self.assertIsNotNone(record, operation)
        kind, spec, state = record
        self.assertEqual(kind, KIND)
        self.assertEqual(json.loads(spec)["operation_id"], identifier)
        body = json.loads(state)
        self.assertEqual(body["lifecycle"], "Prepared", body)
        self.assertTrue(body["started_effect_ref"]["resource_id"], body)
        depot = self.wrapper / ".entry-retirements"
        stage = depot / f"mkdir-{identifier}"
        location = body["location"]
        paths = {"wrapper": self.wrapper, "root": self.root, "parent": self.parent, "depot": depot}
        for name, path in paths.items():
            self.assertEqual(tuple(location["parents"][name]), identity(path), location)
        self.assertEqual(tuple(location["entry"]["identity"]), identity(self.leaf if published else stage), location)
        return stage

    def verify_stopped(self):
        checked = json.loads(subprocess.check_output(self.n.daemon + ["verify"], timeout=20))
        self.assertEqual(checked["sqlite_integrity"], "Verified", checked)
        self.assertFalse(checked["migration_required"], checked)

    def holds_volume(self, operation):
        with closing(sqlite3.connect(self.database.as_uri() + "?mode=ro", uri=True, timeout=10)) as db:
            return db.execute("SELECT 1 FROM holds WHERE volume=? AND holder=?",
                              (self.volume["resource_id"], operation["resource_id"])).fetchone() is not None

    def receipt(self, task):
        artifact, = task["result"]["artifact_refs"]
        return self.n.call("Artifact.Read", {"reference": artifact})

    def assert_unknown(self, task, operation):
        self.deny("OutcomeUnknown", "Operation.Reconcile", target(operation))
        self.assertEqual(self.n.wait(task["task_id"]), task)
        self.assertEqual(self.n.get(self.volume["resource_id"])["reference"], self.volume)

    def assert_replay(self, request, accepted):
        self.assertEqual(self.n.call("Task.Submit", request), {**accepted, "deduplicated": True})

    def test_same_group_setgid_inheritance_survives_publication(self):
        self.parent.chmod(0o2700)
        self.assertEqual(self.parent.stat().st_mode & 0o7777, 0o2700)
        self.assertEqual(self.parent.stat().st_gid, os.getegid())
        receipt = self.n.perform(TOOL, {"target": target(self.volume), "path": self.path})
        self.assertEqual(receipt["receipt"], {"path": self.path})
        self.assertEqual(self.leaf.stat().st_gid, self.parent.stat().st_gid)
        self.assertEqual(self.leaf.stat().st_mode & 0o7777, 0o2700)
        self.assert_protected()

    def test_inherited_default_acl_is_refused_before_publication(self):
        try:
            os.setxattr(self.parent, "system.posix_acl_default", private_acl())
        except OSError as error:
            if error.errno == errno.EOPNOTSUPP:
                self.skipTest("requires a filesystem with Linux POSIX ACLs")
            raise
        self.assertTrue(os.getxattr(self.parent, "system.posix_acl_default"))
        task = self.n.wait(self.n.submit(TOOL, {"target": target(self.volume), "path": self.path}), autoapprove=True)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"], task["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "UnsupportedCapability"), task)
        self.assertFalse(self.leaf.exists())
        self.assertEqual(self.n.get(self.volume["resource_id"])["reference"], self.volume)
        self.assert_protected()

    def test_lost_catalogue_reconciles_only_exact_empty_directory_and_preserves_neighbours(self):
        n = self.n
        request = self.prepare()
        accepted = n.call("Task.Submit", request)
        job = accepted["task_id"]
        self.assertEqual(n.wait(job)["lifecycle"], "WaitingApproval")
        with lose_volume_receipt(self.database, self.volume):
            n.approve(job)
            original, operation = self.paused(job)
            stage = self.stage(operation, published=True)
            exact = identity(self.leaf)
            self.assertFalse(stage.exists())
            self.assertEqual(list(self.leaf.iterdir()), [])
            self.assertEqual(n.get(self.volume["resource_id"])["reference"], self.volume)
            self.assert_protected()
            n.stop(signal.SIGKILL)
        self.verify_stopped()
        n.start()
        self.assertEqual(self.paused(job), (original, operation))
        self.assertEqual(identity(self.leaf), exact)
        self.assert_replay(request, accepted)
        self.deny("InvalidArgument", "Operation.Reconcile", {**target(operation), "evidence_ref": self.volume})
        other, _ = n.agent(["task.read"])
        self.deny("PermissionDenied", "Operation.Reconcile", target(operation), other["token"])
        self.assertTrue(self.holds_volume(operation))
        held = n.wait(n.submit("volume.read", {**target(self.volume), "name": "keep"}))
        self.assertEqual((held["lifecycle"], held["conditions"][0]["type"]), ("Failed", "Conflict"), held)
        self.deny("OutcomeUnknown", "Task.Retry", {**target(n.get(job)["reference"]), "key": key()})

        saved_parent = self.parent.with_name("original-parent")
        parent_identity = identity(self.parent)
        self.parent.rename(saved_parent)
        saved_leaf = saved_parent / self.leaf.name
        try:
            self.parent.mkdir(mode=0o700)
            self.assertNotEqual(identity(self.parent), parent_identity)
            saved_leaf.rename(self.leaf)
            try:
                self.assertEqual(identity(self.leaf), exact)
                self.assert_unknown(original, operation)
            finally:
                self.leaf.rename(saved_leaf)
        finally:
            if self.parent.exists():
                self.parent.rmdir()
            saved_parent.rename(self.parent)

        parked = self.leaf.with_name("original-directory")
        self.leaf.rename(parked)
        try:
            self.assert_unknown(original, operation)
            self.leaf.mkdir(mode=0o700)
            self.assertNotEqual(identity(self.leaf), exact)
            self.assert_unknown(original, operation)
            self.assertTrue(self.leaf.is_dir())
        finally:
            if self.leaf.exists():
                self.leaf.rmdir()
            parked.rename(self.leaf)

        stamp = self.leaf.stat()
        foreign = self.leaf / "foreign"
        foreign.write_bytes(b"Do not remove these bytes.")
        try:
            self.assert_unknown(original, operation)
            self.assertEqual(foreign.read_bytes(), b"Do not remove these bytes.")
        finally:
            foreign.unlink()
            os.utime(self.leaf, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        try:
            os.setxattr(self.leaf, "system.posix_acl_access", private_acl())
        except OSError as error:
            if error.errno != errno.EOPNOTSUPP:
                raise
        else:
            try:
                self.assertEqual(self.leaf.stat().st_mode & 0o7777, stamp.st_mode & 0o7777)
                self.assert_unknown(original, operation)
                self.assertTrue(os.getxattr(self.leaf, "system.posix_acl_access"))
            finally:
                os.removexattr(self.leaf, "system.posix_acl_access")
        self.assert_protected()
        self.assertFalse(stage.exists())

        settled = n.call("Operation.Reconcile", target(operation))
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"), settled)
        done = n.wait(job)
        self.assertEqual((done["lifecycle"], done["current_run_id"]), ("Succeeded", original["current_run_id"]))
        final_volume = n.get(self.volume["resource_id"])["reference"]
        self.assertNotEqual(final_volume, self.volume)
        self.assertEqual(self.receipt(done), {"volume_ref": final_volume, "receipt": {"path": self.path}})
        self.assert_replay(request, accepted)
        self.assertEqual(n.wait(job), done)
        self.assertEqual(identity(self.leaf), exact)
        self.assertEqual(list(self.leaf.iterdir()), [])
        self.assert_protected()
        self.assertFalse(self.holds_volume(operation))
        self.assertEqual(n.perform("volume.read", {**target(final_volume), "name": "keep"})["content"],
                         self.sentinel.read_text())
        n.stop()
        self.verify_stopped()

    @unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_MKDIR_RENAME_FAULT") == "1" and shutil.which("strace"),
                         "requires interruption after actual private mkdirat")
    def test_killed_private_allocation_preserves_unexpected_contents(self):
        n = self.n
        n.stop()
        normal, trace = n.daemon, n.base / "volume-mkdir-allocate.strace"
        # The held worker issues mkdirat for the private depot and then for
        # the reserved stage. The second syscall succeeds before its delay.
        n.daemon = ["strace", "-D", "-f", "-o", str(trace), "-e", "trace=mkdirat,renameat2",
                    "-e", "inject=mkdirat:delay_exit=8000ms:when=2", "--", *normal]
        try:
            n.start()
        finally:
            n.daemon = normal
        self.assertTrue(os.path.samefile(f"/proc/{n.process.pid}/exe", normal[0]))
        request = self.prepare()
        accepted = n.call("Task.Submit", request)
        job = accepted["task_id"]
        awaiting = n.wait(job)
        self.assertEqual(awaiting["lifecycle"], "WaitingApproval")
        identifier = awaiting["operation_refs"][0]["resource_id"]
        stage = self.wrapper / ".entry-retirements" / f"mkdir-{identifier}"
        n.approve(job)
        deadline = time.monotonic() + 6
        while not stage.is_dir() and time.monotonic() < deadline:
            time.sleep(0.03)
        self.assertTrue(stage.is_dir(), trace.read_text())
        state = n.get(f"volume-directory-creation-{identifier}")["value"]
        self.assertEqual(state["lifecycle"], "Allocating", state)
        self.assertNotIn("location", state)
        self.assertFalse(self.leaf.exists())
        exact = identity(stage)
        n.stop(signal.SIGKILL)
        self.assertIn(f'"{stage.name}"', trace.read_text())
        self.verify_stopped()
        foreign = stage / "retain"
        foreign.write_bytes(b"unexpected data in the reserved private directory")

        n.start()
        settled = n.wait(job)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"], settled["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "LeaseLost"), settled)
        self.assertFalse(self.holds_volume(settled["operation_refs"][0]))
        # An attempted physical disposal commits its normal retry delay while
        # through-inode rmdir refuses the nonempty directory.
        deadline = time.monotonic() + 8
        retry_scheduled = False
        while time.monotonic() < deadline:
            with closing(sqlite3.connect(self.database.as_uri() + "?mode=ro", uri=True, timeout=10)) as db:
                row = db.execute("SELECT phase,eligible_at FROM volume_mkdir_stages WHERE operation=?",
                                 (identifier,)).fetchone()
            if row and row[0] == "Cleanup" and datetime.fromisoformat(row[1]) > datetime.now(timezone.utc):
                retry_scheduled = True
                break
            time.sleep(0.05)
        self.assertTrue(retry_scheduled, "the private disposal did not record its unsuccessful attempt")
        self.assertEqual(identity(stage), exact)
        self.assertEqual(foreign.read_bytes(), b"unexpected data in the reserved private directory")
        self.assertFalse(self.leaf.exists())
        foreign.unlink()
        deadline = time.monotonic() + 12
        while stage.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(stage.exists())
        self.assert_replay(request, accepted)
        self.assert_protected()
        n.stop()
        self.verify_stopped()

    @unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_MKDIR_RENAME_FAULT") == "1" and shutil.which("strace"),
                         "requires actual renameat2 fault injection and process kill")
    def test_killed_rename_reconciles_not_applied_and_does_not_create_target(self):
        n = self.n
        n.stop()
        normal, trace = n.daemon, n.base / "volume-mkdir-rename.strace"
        n.daemon = ["strace", "-D", "-f", "-o", str(trace), "-e", "trace=renameat2",
                    "-e", "inject=renameat2:error=EINTR:signal=SIGKILL:when=1", "--", *normal]
        try:
            n.start()
        finally:
            n.daemon = normal
        self.assertTrue(os.path.samefile(f"/proc/{n.process.pid}/exe", normal[0]))
        request = self.prepare()
        accepted = n.call("Task.Submit", request)
        job = accepted["task_id"]
        self.assertEqual(n.wait(job)["lifecycle"], "WaitingApproval")
        try:
            n.approve(job)
        except (ConnectionError, FileNotFoundError, struct.error):
            # The deliberate kill can interrupt the response after approval commits.
            pass
        self.assertEqual(n.process.wait(25), -signal.SIGKILL)
        self.verify_stopped()
        n.start()
        original, operation = self.paused(job)
        stage = self.stage(operation, published=False)
        self.assertFalse(self.leaf.exists())
        self.assertTrue(stage.is_dir())
        self.assertEqual(list(stage.iterdir()), [])
        lines = trace.read_text().splitlines()
        attempted = [line for line in lines if "renameat2(" in line and f'"{stage.name}"' in line
                     and f'"{self.leaf.name}"' in line and "RENAME_NOREPLACE" in line]
        self.assertEqual(len(attempted), 1, lines)
        thread = attempted[0].split(maxsplit=1)[0]
        self.assertTrue(any(line.split(maxsplit=1)[0] == thread and "killed by SIGKILL" in line
                            for line in lines if line.strip()), lines)

        settled = n.call("Operation.Reconcile", target(operation))
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Failed", "Known"), settled)
        failed = n.wait(job)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                         ("Failed", "Known", "LeaseLost"), failed)
        self.assertEqual(failed["current_run_id"], original["current_run_id"])
        self.assertEqual(self.receipt(failed), {"path": self.path, "created": False})
        deadline = time.monotonic() + 12
        while (stage.exists() or self.holds_volume(operation)) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(stage.exists())
        self.assertFalse(self.holds_volume(operation))
        self.assertFalse(self.leaf.exists())
        self.assertEqual(n.get(self.volume["resource_id"])["reference"], self.volume)
        self.assert_protected()
        self.assert_replay(request, accepted)
        self.assertEqual(n.wait(job), failed)
        self.assertFalse(self.leaf.exists())

        new = n.perform(TOOL, {"target": target(self.volume), "path": self.path})
        self.assertEqual(new["receipt"], {"path": self.path})
        self.assertEqual(list(self.leaf.iterdir()), [])
        self.assert_protected()
        n.stop()
        self.verify_stopped()


@unittest.skipUnless(PRIOR_BIN, "requires genuine format 119 binaries")
class VolumeMkdirUpgrade(unittest.TestCase):
    def test_legacy_unknown_stays_fenced_and_new_writer_rejects_old_format_binary(self):
        n = Node(PRIOR_BIN)
        self.addCleanup(n.close)
        volume = n.perform("volume.create", {"name": "legacy-mkdir"})
        root = n.state / "volumes" / volume["resource_id"] / "data"
        directory = root / "legacy-directory"
        request = n.prepare(TOOL, {"target": target(volume), "path": directory.name})
        accepted = n.call("Task.Submit", request)
        job = accepted["task_id"]
        self.assertEqual(n.wait(job)["lifecycle"], "WaitingApproval")
        with lose_volume_receipt(n.state / "state.db", volume):
            task = n.wait(job, autoapprove=True)
            self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Paused", "Unknown"), task)
            exact = identity(directory)
            n.stop(signal.SIGKILL)

        previous = n.daemon[:]
        old = json.loads(subprocess.check_output(previous + ["verify"], timeout=20))
        self.assertEqual(old["database_format"], 119)
        current_bin = os.path.abspath(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        n.daemon[0] = os.path.join(current_bin, "psiosd")
        preview = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertTrue(preview["migration_required"])
        n.start()
        self.assertEqual(n.wait(job), task)
        operation, = task["operation_refs"]
        with self.assertRaises(Failure) as refused:
            n.call("Operation.Reconcile", target(operation))
        self.assertEqual(refused.exception.code, "RecoveryRequired")
        self.assertEqual(identity(directory), exact)
        self.assertEqual(n.call("Task.Submit", request), {**accepted, "deduplicated": True})
        self.assertEqual(n.wait(job), task)
        other = n.perform("volume.create", {"name": "new-mkdir-after-upgrade"})
        other_result = n.perform(TOOL, {"target": target(other), "path": "new-directory"})
        self.assertEqual(other_result["receipt"], {"path": "new-directory"})
        n.stop()
        checked = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertGreaterEqual(checked["database_format"], 120)
        self.assertFalse(checked["migration_required"])
        writer = subprocess.run(previous + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(writer.returncode, 0, writer)
        self.assertIn("SchemaVersionUnsupported", writer.stderr)
