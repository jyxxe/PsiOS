"""Import production backup exports and exercise their original restore boundary."""
from contextlib import closing
import json
import os
import shutil
import signal
import sqlite3
import stat
import subprocess
import unittest

from support import Failure, Node, target


class DetachedBackupIntake(unittest.TestCase):
    def setUp(self):
        binaries = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")
        self.n = Node(binaries)
        self.addCleanup(self.n.close)
        source = Node(binaries)
        self.addCleanup(source.close)
        self.assertNotEqual(source.call("Node.Get", {})["reference"]["resource_id"],
                            self.n.call("Node.Get", {})["reference"]["resource_id"])
        volume = source.perform("volume.create", {"name": "detached-source"})
        volume = source.perform("volume.write", {**target(volume), "name": "config", "content": "original"})["volume_ref"]
        self.volume = source.get(volume["resource_id"])
        tree = source.state / "volumes" / volume["resource_id"] / "data"
        (tree / "nested").mkdir()
        self.blob = bytes(range(256)) * 550
        data = tree / "nested" / "data"
        data.write_bytes(self.blob)
        data.chmod(0o640)
        self.mtime = 1_600_000_000_120_000_004
        os.utime(data, ns=(self.mtime, self.mtime))
        self.backup = source.perform("backup.create", target(volume))
        self.digest = source.get(self.backup["resource_id"])["value"]["manifest_digest"]
        self.archive = self.n.base / "offline-backup"
        self.assertEqual(self.export(source, self.backup, self.archive)["manifest_digest"], self.digest)
        self.published = self.n.state / "backups" / self.backup["resource_id"]
        self.blob_name = next(f"blob-{index}" for index, entry in enumerate(
            json.loads((self.archive / "manifest.json").read_bytes())["entries"],
        ) if entry["path"] == "nested/data")
        source.close()
        self.assertFalse(source.state.exists())

    def export(self, node, reference, output):
        agent = node.call("Agent.Register", {"name": "exporter"})
        node.call("Grant.Issue", {
            "principal_id": agent["principal_id"], "actions": ["backup.export"], "units": 2,
            "ttl_seconds": 90, "tool_targets": [reference["resource_id"]],
        })
        token = node.base / "export.token"
        token.touch(mode=0o600)
        token.write_text(agent["token"])
        data = subprocess.check_output([
            node.binaries / "psiosctl", "--socket", node.socket, "--token-file", token,
            "backup-export", "--output", output,
        ], input=json.dumps(reference).encode(), timeout=30)
        return json.loads(data)

    def register(self, *, directory=None, digest=None, error=None):
        result = subprocess.run(self.n.daemon + [
            "import-backup", "--directory", directory or self.archive,
            "--manifest-digest", digest or self.digest,
        ], capture_output=True, timeout=30)
        if error:
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")
            self.assertIn(error.encode(), result.stderr)
            return None
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_import_without_original_authority_then_approve_restore_and_activate(self):
        n = self.n
        self.register(error="Conflict")
        self.assertFalse(self.published.exists())
        n.stop()
        self.register(digest="sha256:" + "0" * 64, error="PreconditionFailed")
        imported = self.register()
        same = self.register()
        self.assertEqual(same, dict(imported, reused=True))
        self.assertFalse(imported["reused"])
        self.assertEqual(imported["manifest_digest"], self.digest)
        self.assertEqual(imported["reference"]["resource_id"], self.backup["resource_id"])
        self.assertEqual((self.published / self.blob_name).read_bytes(), self.blob)
        n.start()
        trusted = n.get(imported["reference"]["resource_id"])
        self.assertEqual(trusted["reference"], imported["reference"])
        self.assertEqual(trusted["resource"]["spec"]["source_data_incarnation"], self.volume["value"]["data_incarnation"])

        viewer, grant = n.agent(["task.submit", "task.read"])
        with self.assertRaises(Failure) as denied:
            n.prepare("backup.restore", target(imported["reference"]), token=viewer["token"],
                      delegation=grant["delegation_ref"]["resource_id"])
        self.assertEqual(denied.exception.code, "PermissionDenied")
        restored_task = n.submit("backup.restore", target(imported["reference"]))
        task = n.wait(restored_task)
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        volume = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        local = n.get(volume["resource_id"])
        self.assertNotEqual(volume["resource_id"], self.volume["reference"]["resource_id"])
        self.assertNotEqual(local["value"]["data_incarnation"], self.volume["value"]["data_incarnation"])
        self.assertEqual(local["resource"]["spec"]["storage_restore_phase"], "AwaitingActivation")
        data = n.state / "volumes" / volume["resource_id"] / "data" / "nested" / "data"
        self.assertEqual(data.read_bytes(), self.blob)
        self.assertEqual((stat.S_IMODE(data.stat().st_mode), data.stat().st_mtime_ns), (0o640, self.mtime))
        write = {**target(volume), "name": "config", "content": "new value"}
        blocked = n.wait(n.submit("volume.write", write), autoapprove=True)
        self.assertEqual(blocked["conditions"][0]["type"], "PreconditionFailed")
        activation = n.submit("volume.activate", target(volume))
        self.assertEqual(n.wait(activation)["lifecycle"], "WaitingApproval")
        n.approve(activation)
        self.assertEqual(n.wait(activation)["lifecycle"], "Succeeded")
        active = n.get(volume["resource_id"])["reference"]
        self.assertEqual(n.perform("volume.read", {**target(active), "name": "config"})["content"], "original")
        n.perform("volume.write", {**write, **target(active)})

        exported = self.n.base / "second-export"
        self.assertEqual(self.export(n, imported["reference"], exported)["manifest_digest"], self.digest)
        self.assertEqual({entry.name: entry.read_bytes() for entry in self.archive.iterdir()},
                         {entry.name: entry.read_bytes() for entry in exported.iterdir()})

    def test_interrupted_registration_reuses_only_the_witnessed_verified_directory(self):
        n = self.n
        n.stop(signal.SIGKILL)
        bad = n.base / "bad-input"
        shutil.copytree(self.archive, bad)
        (bad / self.blob_name).write_bytes(b"\0" * len(self.blob))
        self.register(directory=bad, error="PreconditionFailed")
        self.assertEqual(list(self.published.parent.iterdir()), [])
        self.published.mkdir(mode=0o700)
        self.register(error="Conflict")
        self.published.rmdir()

        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("""
                CREATE TRIGGER fail_intake_catalog BEFORE INSERT ON versions
                WHEN NEW.kind='Backup' BEGIN
                    SELECT RAISE(ABORT, 'test published intake settlement failure');
                END
            """)
        self.register(error="StorageUnavailable")
        inode = self.published.stat().st_ino
        self.assertEqual(list(self.published.parent.iterdir()), [self.published])
        n.start()
        with self.assertRaises(Failure) as missing:
            n.get(self.backup["resource_id"])
        self.assertEqual(missing.exception.code, "NotFound")
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER fail_intake_catalog")
        staged = n.base / "published-copy"
        self.published.rename(staged)
        shutil.copytree(staged, self.published)
        self.register(error="RecoveryRequired")
        shutil.rmtree(self.published)
        staged.rename(self.published)
        self.archive.rename(n.base / "archived-elsewhere")
        reused = self.register()
        self.assertTrue(reused["reused"])
        self.assertEqual(self.published.stat().st_ino, inode)
        self.assertEqual(self.register(), reused)
        n.start()
        self.assertEqual(n.get(self.backup["resource_id"])["reference"], reused["reference"])
        audit = [event["decision"] for event in n.call("Audit.List", {"limit": 128})
                 if event["method"] == "Backup.ImportDetached"]
        self.assertEqual(audit, ["Prepared", "Imported"])
