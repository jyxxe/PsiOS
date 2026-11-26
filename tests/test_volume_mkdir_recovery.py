"""Reconcile a real nested mkdir without replaying it or adopting a substitute."""

from contextlib import closing, contextmanager
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

    @contextmanager
    def lose_volume_receipt(self):
        with closing(sqlite3.connect(self.database, timeout=10)) as db, db:
            db.execute(f"""CREATE TRIGGER lose_mkdir_volume_revision BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.id='{self.volume['resource_id']}'
                BEGIN SELECT RAISE(ABORT,'fixture withheld the mkdir catalogue receipt'); END""")
        try:
            yield
        finally:
            with closing(sqlite3.connect(self.database, timeout=10)) as db, db:
                db.execute("DROP TRIGGER lose_mkdir_volume_revision")

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
        with self.lose_volume_receipt():
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
