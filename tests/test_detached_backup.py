"""Verify and extract a real Core backup without its authority or service."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import unittest

from support import Node, target


def sha(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def tree_identity(directory):
    return {path.name: (stat.S_IMODE(path.stat().st_mode), path.stat().st_mtime_ns, sha(path.read_bytes()))
            for path in directory.iterdir()}


class DetachedBackupTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.output = self.node.base / "outputs"
        self.output.mkdir(mode=0o700)
        self.volume = self.node.perform("volume.create", {"name": "offline-data"})
        self.data = self.node.state / "volumes" / self.volume["resource_id"] / "data"
        self.contents = {"first": b"\x00\x01\xff\x80", "nested/second": b"value"}
        (self.data / "nested").mkdir()
        for index, (name, contents) in enumerate(self.contents.items()):
            path = self.data / name
            path.write_bytes(contents)
            path.chmod(0o640 + index)
            timestamp = 1_700_000_000_100_400_125 + index
            os.utime(path, ns=(timestamp, timestamp))
        nested = self.data / "nested"
        nested.chmod(0o555)
        timestamp = 1_700_000_000_400_600_321
        os.utime(nested, ns=(timestamp, timestamp))
        self.metadata = {name: ((self.data / name).stat().st_mode, (self.data / name).stat().st_mtime_ns)
                         for name in (*self.contents, "nested")}

    def detached(self):
        backup = self.node.perform("backup.create", target(self.volume))
        obj = self.node.get(backup["resource_id"])
        digest = obj["value"]["manifest_digest"]
        source = self.node.state / "backups" / backup["resource_id"]
        copy = self.node.base / "removable-copy"
        shutil.copytree(source, copy)
        self.node.stop()
        self.node.state.rename(self.node.base / "unmounted-authority")
        self.node.witness.rename(self.node.base / "unmounted-witness")
        self.node.config.unlink()
        return copy, digest, backup["resource_id"]

    def command(self, source, digest, output=None):
        command = [self.node.binaries / "psiosd", "--config", self.node.config, "verify-backup",
                   "--directory", source, "--manifest-digest", digest]
        if output is not None:
            command += ["--restore-to", output]
        return subprocess.run(command, capture_output=True, text=True, timeout=20)

    def test_verified_recovery_on_a_separate_path(self):
        source, digest, backup = self.detached()
        for member in source.iterdir():
            member.chmod(0o444)
        source.chmod(0o555)
        snapshot = tree_identity(source)
        check = self.command(source, digest)
        self.assertEqual(check.returncode, 0, check.stderr)
        verified = json.loads(check.stdout)
        self.assertEqual((verified["backup_id"], verified["manifest_digest"], verified["entries"], verified["bytes"]),
                         (backup, digest, 3, "9"))
        self.assertEqual(verified["source_ref"], self.volume)
        self.assertNotIn("restored_directory", verified)

        output = self.output / "new"
        result = self.command(source, digest, output)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["restored_directory"], str(output))
        self.assertIn("restore_proof", report)
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o700)
        for name, contents in self.contents.items():
            self.assertEqual((output / name).read_bytes(), contents)
        for name, expected in self.metadata.items():
            actual = (output / name).stat()
            self.assertEqual((actual.st_mode, actual.st_mtime_ns), expected)
        self.assertEqual(tree_identity(source), snapshot)

    def test_forged_manifest_extra_members_and_damaged_payload_leave_no_output(self):
        source, digest, _ = self.detached()
        manifest = source / "manifest.json"
        original = manifest.read_bytes()
        output = self.output / "safe"
        wrong = self.command(source, "sha256:" + "0" * 64, output)
        self.assertNotEqual(wrong.returncode, 0)
        self.assertIn("manifest integrity", wrong.stderr)

        extra = source / "blob-99"
        extra.write_bytes(b"extra")
        unexpected = self.command(source, digest, output)
        self.assertNotEqual(unexpected.returncode, 0)
        self.assertIn("file set", unexpected.stderr)
        extra.unlink()

        mutated = json.loads(original)
        mutated["entries"][0]["mode"] = 0o4755
        altered = json.dumps(mutated).encode()
        manifest.write_bytes(altered)
        unsafe = self.command(source, sha(altered), output)
        self.assertNotEqual(unsafe.returncode, 0)
        self.assertIn("entry tree", unsafe.stderr)
        manifest.write_bytes(original)

        last_blob = sorted(source.glob("blob-*"))[-1]
        old_data = last_blob.read_bytes()
        last_blob.unlink()
        external = self.node.base / "outside-copy"
        external.write_bytes(old_data)
        for link in (last_blob.symlink_to, last_blob.hardlink_to):
            link(external)
            rejected = self.command(source, digest, output)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("changed type or filesystem", rejected.stderr)
            last_blob.unlink()
        self.assertEqual(external.read_bytes(), old_data)
        last_blob.write_bytes(b"XXXXX")
        damaged = self.command(source, digest, output)
        self.assertNotEqual(damaged.returncode, 0)
        self.assertIn("content integrity", damaged.stderr)
        self.assertEqual(list(self.output.iterdir()), [])
        self.assertEqual(last_blob.read_bytes(), b"XXXXX")

    def test_concurrent_extractors_publish_once(self):
        source, digest, _ = self.detached()
        output = self.output / "winner"
        with ThreadPoolExecutor(max_workers=2) as workers:
            first = workers.submit(self.command, source, digest, output)
            second = workers.submit(self.command, source, digest, output)
            results = [first.result(), second.result()]
        self.assertEqual(sum(result.returncode == 0 for result in results), 1, results)
        self.assertEqual(list(self.output.iterdir()), [output])
        for name, contents in self.contents.items():
            self.assertEqual((output / name).read_bytes(), contents)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_DETACHED_TARGET"), "requires a separate bounded filesystem")
    def test_full_target_refuses_publication_and_reclaims_staging(self):
        target_root = Path(os.environ["PSIOS_TEST_DETACHED_TARGET"])
        with (self.data / "large").open("xb") as handle:
            os.posix_fallocate(handle.fileno(), 0, 2 * 1024 * 1024)
        source, digest, _ = self.detached()
        result = self.command(source, digest, target_root / "refused")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("insufficient destination blocks", result.stderr)
        self.assertEqual(list(target_root.iterdir()), [])
