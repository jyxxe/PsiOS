"""Authorize and verify real chunked exports, then recover independently."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import stat
import subprocess
import unittest

from support import Failure, Node, target
from test_detached_backup import sha


class BackupExportTests(unittest.TestCase):
    def setUp(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.node = node
        self.addCleanup(node.close)
        self.output = node.base / "exports"
        self.output.mkdir(mode=0o700)
        volume = node.perform("volume.create", {"name": "export-source"})
        self.contents = {"nested/empty": b"", "nested/binary": bytes(range(256)) * 1100}
        tree = node.state / "volumes" / volume["resource_id"] / "data"
        (tree / "nested").mkdir()
        for name, data in self.contents.items():
            (tree / name).write_bytes(data)
            (tree / name).chmod(0o640)
        self.backup = node.perform("backup.create", target(volume))
        self.source = node.state / "backups" / self.backup["resource_id"]
        self.manifest = (self.source / "manifest.json").read_bytes()
        self.index = next(i for i, e in enumerate(json.loads(self.manifest)["entries"]) if e["path"] == "nested/binary")
        self.reference = node.base / "reference.json"
        self.reference.write_text(json.dumps(self.backup))
        self.agent = node.call("Agent.Register", {"name": "backup-export-reader"})
        self.token = node.base / "export.token"
        self.token.touch(mode=0o600)
        self.token.write_text(self.agent["token"])
        self.scope = self.backup["resource_id"]

    def grant(self, *ids):
        request = {"principal_id": self.agent["principal_id"], "actions": ["backup.export"],
                   "units": 2, "ttl_seconds": 300}
        if ids:
            request["tool_targets"] = list(ids)
        return self.node.call("Grant.Issue", request)

    def read(self, *, member=None, offset=0, length=128 * 1024, token=None, reference=None):
        return self.node.call("Backup.Export", {"reference": reference or self.backup,
            "member": member or {"kind": "Manifest"}, "offset": offset, "length": length}, token)

    def member(self):
        return {"kind": "Blob", "index": self.index}

    def export(self, output):
        return subprocess.run([self.node.binaries / "psiosctl", "--socket", self.node.socket,
            "--token-file", self.token, "backup-export", "--output", output, self.reference],
            capture_output=True, text=True, timeout=30)

    def test_version_scoped_export_and_repeat_authorization(self):
        node = self.node
        token = self.agent["token"]
        for caller in (None, token):
            with self.assertRaises(Failure) as missing:
                self.read(token=caller)
            self.assertEqual(missing.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as broad:
            self.grant()
        self.assertEqual(broad.exception.code, "InvalidArgument")
        self.grant("local")
        with self.assertRaises(Failure) as outside:
            self.read(token=token)
        self.assertEqual(outside.exception.code, "PermissionDenied")
        approved = self.grant(self.scope)
        with self.assertRaises(Failure) as stale:
            self.read(token=token, reference=dict(self.backup, digest="sha256:" + "0" * 64))
        self.assertEqual(stale.exception.code, "PreconditionFailed")
        with self.assertRaises(Failure) as oversize:
            self.read(token=token, length=128 * 1024 + 1)
        self.assertEqual(oversize.exception.code, "InvalidArgument")
        with self.assertRaises(Failure) as outside:
            self.read(token=token, offset=len(self.manifest) + 1)
        self.assertEqual(outside.exception.code, "InvalidArgument")
        with self.assertRaises(Failure) as directory:
            self.read(token=token, member={"kind": "Blob", "index": 0})
        self.assertEqual(directory.exception.code, "NotFound")
        self.assertEqual(base64.b64decode(self.read(token=token)["data_base64"]), self.manifest)
        first = self.read(token=token, member=self.member())
        expected = self.contents["nested/binary"]
        self.assertEqual(first["digest"], sha(expected))
        self.assertEqual(base64.b64decode(first["data_base64"]), expected[:first["next_offset"]])
        node.stop()
        node.start()
        resumed = self.read(token=token, member=self.member(), offset=first["next_offset"])
        self.assertEqual(base64.b64decode(resumed["data_base64"]), expected[first["next_offset"]:resumed["next_offset"]])
        self.node.call("Grant.Revoke", target(approved["grant_ref"]))
        with self.assertRaises(Failure) as revoked:
            self.read(token=token, member=self.member(), offset=resumed["next_offset"])
        self.assertEqual(revoked.exception.code, "PermissionDenied")
        audit = [row for row in node.call("Audit.List", {"limit": 128}) if row["method"] == "Backup.Export"]
        self.assertEqual(sum(row["decision"] == "Allowed" and row["resource"] == self.scope for row in audit), 3)
        self.assertEqual(audit[-1]["decision"], "PermissionDenied")

    def test_cli_atomic_export_can_be_restored_without_source_authority(self):
        self.grant(self.scope)
        out = self.output / "portable"
        with ThreadPoolExecutor(max_workers=2) as threads:
            results = list(threads.map(self.export, [out, out]))
        winner, = (result for result in results if result.returncode == 0)
        self.assertEqual(list(self.output.iterdir()), [out])
        self.assertEqual(stat.S_IMODE(out.stat().st_mode), 0o700)
        self.assertTrue(all(stat.S_IMODE(path.stat().st_mode) == 0o600 for path in out.iterdir()))
        report = json.loads(winner.stdout)
        self.assertEqual((report["backup_ref"], report["manifest_digest"]), (self.backup, sha(self.manifest)))
        self.assertEqual({p.name: p.read_bytes() for p in out.iterdir()},
                         {p.name: p.read_bytes() for p in self.source.iterdir()})
        node = self.node
        node.stop()
        node.state.rename(node.base / "absent-authority")
        node.witness.rename(node.base / "absent-witness")
        restored = self.output / "restored"
        result = subprocess.run([node.binaries / "psiosd", "verify-backup", "--directory", out,
            "--manifest-digest", report["manifest_digest"], "--restore-to", restored], capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name, data in self.contents.items():
            path = restored / name
            self.assertEqual(path.read_bytes(), data)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o640)

    def test_cached_member_tampering_and_deleted_backup_refuse_export(self):
        self.grant(self.scope)
        token = self.agent["token"]
        first = self.read(token=token, member=self.member())
        blob = self.source / f"blob-{self.index}"
        before = blob.stat()
        with blob.open("r+b") as contents:
            contents.seek(-1, os.SEEK_END)
            contents.write(b"\x00")
        os.utime(blob, ns=(before.st_atime_ns, before.st_mtime_ns))
        with self.assertRaises(Failure) as damaged:
            self.read(token=token, member=self.member(), offset=first["next_offset"])
        self.assertEqual(damaged.exception.code, "PreconditionFailed")
        self.assertIn("integrity", str(damaged.exception))
        result = self.export(self.output / "damaged")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(list(self.output.iterdir()), [])
        self.node.perform("backup.delete", target(self.backup))
        with self.assertRaises(Failure) as deleted:
            self.read(token=token)
        self.assertEqual(deleted.exception.code, "PreconditionFailed")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_EXPORT_TARGET"), "requires a bounded export filesystem")
    def test_export_capacity_failure_removes_staging(self):
        self.grant(self.scope)
        target_root = Path(os.environ["PSIOS_TEST_EXPORT_TARGET"])
        result = self.export(target_root / "insufficient")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("QuotaExceeded", result.stderr)
        self.assertEqual(list(target_root.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
