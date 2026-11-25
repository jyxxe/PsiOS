"""Authorize and verify real chunked exports, then recover independently."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import time
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
        self.volume = node.perform("volume.create", {"name": "export-source"})
        self.contents = {"nested/empty": b"", "nested/binary": bytes(range(256)) * 1100}
        self.tree = node.state / "volumes" / self.volume["resource_id"] / "data"
        (self.tree / "nested").mkdir()
        for name, data in self.contents.items():
            (self.tree / name).write_bytes(data)
            (self.tree / name).chmod(0o640)
        self.reference = node.base / "reference.json"
        self.agent = node.call("Agent.Register", {"name": "backup-export-reader"})
        self.token = node.base / "export.token"
        self.token.touch(mode=0o600)
        self.token.write_text(self.agent["token"])
        self.create_backup()

    def create_backup(self):
        self.backup = self.node.perform("backup.create", target(self.volume))
        self.scope = self.backup["resource_id"]
        self.source = self.node.state / "backups" / self.scope
        self.manifest = (self.source / "manifest.json").read_bytes()
        self.index = next(i for i, e in enumerate(json.loads(self.manifest)["entries"]) if e["path"] == "nested/binary")
        self.reference.write_text(json.dumps(self.backup))

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

    def export_args(self, output, *flags):
        return [self.node.binaries / "psiosctl", "--socket", self.node.socket,
            "--token-file", self.token, "backup-export", "--output", output, *flags, self.reference]

    def export(self, output, *flags):
        return subprocess.run(self.export_args(output, *flags), capture_output=True, text=True, timeout=45)

    def stage_for(self, output):
        identity = sha(json.dumps(["psios-backup-export-v1", output.name, self.backup],
                                  sort_keys=True, separators=(",", ":")).encode()).split(":", 1)[1]
        return output.parent / f".psios-backup-export-{identity}"

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

    def test_interrupted_export_resumes_verified_prefix_and_rechecks_grant(self):
        node = self.node
        data = bytes(range(256)) * (8 * 1024 * 1024 // 256)
        (self.tree / "nested/binary").write_bytes(data)
        self.create_backup()
        grant = self.grant(self.scope)
        out = self.output / "resumable"
        blob = f"blob-{self.index}"
        first = subprocess.Popen(self.export_args(out, "--resume"), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 30
            prefix = None
            while first.poll() is None and time.monotonic() < deadline:
                prefix = next(self.output.glob(f".psios-backup-export-*/{blob}"), None)
                if prefix and prefix.stat().st_size >= 1024 * 1024:
                    break
                time.sleep(0.015)
            else:
                self.fail("export did not leave a partial staged member")
            first.send_signal(signal.SIGSTOP)
            concurrent = self.export(out, "--resume")
            self.assertNotEqual(concurrent.returncode, 0)
            self.assertIn("Conflict", concurrent.stderr)
        finally:
            if first.poll() is None:
                first.kill()
            first.communicate(timeout=10)
        self.assertEqual(first.returncode, -signal.SIGKILL)
        self.assertFalse(out.exists())
        saved = prefix.read_bytes()
        self.assertLess(len(saved), len(data))
        self.assertEqual(saved, data[:len(saved)])
        inode = prefix.stat().st_ino

        node.call("Grant.Revoke", target(grant["grant_ref"]))
        denied = self.export(out, "--resume")
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn("PermissionDenied", denied.stderr)
        self.assertEqual(prefix.read_bytes(), saved)
        self.grant(self.scope)

        # Clone the interrupted stage under a second output identity and damage its prefix.
        other = self.output / "repaired"
        damaged = self.stage_for(other)
        shutil.copytree(prefix.parent, damaged)
        with (damaged / blob).open("r+b") as staged:
            staged.write(b"\xff")

        offset = int(node.call("Audit.List", {"limit": 128})[-1]["seq"])
        resumed = self.export(out, "--resume")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        audit = node.call("Audit.List", {"after": offset, "limit": 128})
        reads = [row for row in audit if row["method"] == "Backup.Export" and row["decision"] == "Allowed"
                 and row["resource"] == self.scope]
        full_blob_chunks = len(data) // (128 * 1024)
        self.assertLessEqual(len(reads), full_blob_chunks - len(saved) // (128 * 1024) + 3)
        self.assertEqual(sha((out / blob).read_bytes()), sha(data))
        self.assertEqual((out / blob).stat().st_ino, inode)
        self.assertFalse(prefix.parent.exists())

        repaired = self.export(other, "--resume")
        self.assertEqual(repaired.returncode, 0, repaired.stderr)
        self.assertEqual(sha((other / blob).read_bytes()), sha(data))
        self.assertEqual(list(sorted(self.output.iterdir())), [other, out])

        repeated = self.export(out, "--resume")
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        first_report = json.loads(resumed.stdout)
        repeated_report = json.loads(repeated.stdout)
        self.assertGreaterEqual(repeated_report.pop("verified_at"), first_report.pop("verified_at"))
        self.assertEqual(repeated_report, first_report)
        with (out / blob).open("r+b") as published:
            published.write(b"\xff")
        refused = self.export(out, "--resume")
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual((out / blob).stat().st_ino, inode)
        with (out / blob).open("rb") as published:
            self.assertEqual(published.read(1), b"\xff")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_EXPORT_TARGET"), "requires a bounded export filesystem")
    def test_export_capacity_failure_removes_staging(self):
        self.grant(self.scope)
        target_root = Path(os.environ["PSIOS_TEST_EXPORT_TARGET"])
        result = self.export(target_root / "insufficient")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("QuotaExceeded", result.stderr)
        self.assertEqual(list(target_root.iterdir()), [])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_EXPORT_RESUME_TARGET"), "requires a bounded export filesystem")
    def test_resumed_preflight_accounts_for_allocated_prefix(self):
        out = Path(os.environ["PSIOS_TEST_EXPORT_RESUME_TARGET"]) / "resumed-at-capacity"
        data = bytes(range(256)) * (4 * 1024 * 1024 // 256)
        (self.tree / "nested/binary").write_bytes(data)
        self.create_backup()
        self.grant(self.scope)
        staging = self.stage_for(out)
        staging.mkdir(mode=0o700)
        blob = f"blob-{self.index}"
        prefix = staging / blob
        prefix.touch(mode=0o600)
        prefix.write_bytes(data[:len(data) // 2])
        fs = os.statvfs(staging)
        self.assertLess(len(data) // 2, fs.f_bavail * fs.f_frsize)
        self.assertLess(fs.f_bavail * fs.f_frsize, len(data))
        full = self.export(out)
        self.assertNotEqual(full.returncode, 0)
        self.assertIn("QuotaExceeded", full.stderr)
        resumed = self.export(out, "--resume")
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(sha((out / blob).read_bytes()), sha(data))


if __name__ == "__main__":
    unittest.main()
