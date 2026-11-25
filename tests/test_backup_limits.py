"""Exercise configurable backup size with real streaming capture and restore."""
import hashlib
import json
import os
import subprocess
import unittest

from support import Node, target


def checksum(path):
    with path.open("rb") as data:
        return hashlib.file_digest(data, "sha256").hexdigest()


class BackupLimits(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "backup-limits"})
        self.data = self.n.state / "volumes" / self.volume["resource_id"] / "data" / "payload"

    def configure(self, node, limit, *, restart=True):
        node.stop()
        config = json.loads(node.config.read_text())
        config["backup_max_bytes"] = limit
        node.config.write_text(json.dumps(config))
        if restart:
            node.start()
            self.assertEqual(node.call("StoragePool.Get", {"id": "local"})["backup_max_bytes"], str(limit))

    def backup_denied(self):
        failed = self.n.wait(self.n.submit("backup.create", target(self.volume)))
        self.assertEqual((failed["lifecycle"], failed["conditions"][0]["type"]), ("Failed", "QuotaExceeded"))

    def command(self, args, *, error=None, input=None, timeout=45):
        result = subprocess.run(args, input=input, capture_output=True, timeout=timeout)
        if error:
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")
            self.assertIn(error.encode(), result.stderr)
            return None
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def import_command(self, node, directory, digest):
        return node.daemon + ["import-backup", "--directory", directory, "--manifest-digest", digest]

    def manifest_digest(self, reference):
        return self.n.get(reference["resource_id"])["value"]["manifest_digest"]

    def test_lower_limit_refuses_new_capture_and_intake_and_preserves_stored_restore(self):
        n = self.n
        self.data.write_bytes(b"sealed" * 180)
        original = n.perform("backup.create", target(self.volume))
        directory = n.state / "backups" / original["resource_id"]
        digest = self.manifest_digest(original)
        self.configure(n, 256)
        self.backup_denied()
        self.assertEqual(list(directory.parent.iterdir()), [directory])
        check = [n.binaries / "psiosd", "verify-backup", "--directory", directory, "--manifest-digest", digest]
        self.command(check + ["--max-bytes", "256"], error="QuotaExceeded")
        self.assertEqual(self.command(check)["bytes"], "1080")
        restored = n.perform("backup.restore", target(original))
        copied = n.state / "volumes" / restored["resource_id"] / "data" / "payload"
        self.assertEqual(copied.read_bytes(), self.data.read_bytes())

        receiver = Node(n.binaries)
        self.addCleanup(receiver.close)
        self.configure(receiver, 256, restart=False)
        command = self.import_command(receiver, directory, digest)
        self.command(command, error="QuotaExceeded")
        self.assertEqual(list((receiver.state / "backups").iterdir()), [])
        self.configure(receiver, 1080, restart=False)
        accepted = self.command(command)
        self.assertEqual(accepted["manifest_digest"], digest)
        self.configure(receiver, 256, restart=False)
        self.assertEqual(self.command(command), dict(accepted, reused=True))
        for invalid in (0, 1024 ** 4 + 1):
            with self.subTest(invalid=invalid):
                self.configure(receiver, invalid, restart=False)
                self.command(receiver.daemon + ["check"], error="InvalidArgument")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_LARGE_BACKUP") == "1", "requires space for a 256 MiB archive and copies")
    def test_large_backup_streams_from_one_authority_and_restores_on_another(self):
        n = self.n
        size = 256 * 1024 * 1024 + 65536
        with self.data.open("wb") as stream:
            stream.write(b"first extent")
            stream.seek(size - len(b"final extent"))
            stream.write(b"final extent")
        self.backup_denied()
        self.configure(n, size)
        saved = n.perform("backup.create", target(self.volume))
        digest = self.manifest_digest(saved)
        expected = checksum(self.data)
        source = n.state / "backups" / saved["resource_id"]
        self.assertEqual(checksum(source / "blob-0"), expected)

        exporter = n.call("Agent.Register", {"name": "large-backup-export"})
        n.call("Grant.Issue", {"principal_id": exporter["principal_id"], "units": 1, "ttl_seconds": 600,
                              "actions": ["backup.export"], "tool_targets": [saved["resource_id"]]})
        token = n.base / "export.token"
        token.touch(mode=0o600)
        token.write_text(exporter["token"])
        output = n.base / "large-export"
        export = [n.binaries / "psiosctl", "--socket", n.socket, "--token-file", token,
                  "backup-export", "--output", output]
        pinned = json.dumps(saved).encode()
        self.command(export, error="QuotaExceeded", input=pinned)
        self.assertFalse(output.exists())
        receipt = self.command(export + ["--max-bytes", str(size)], input=pinned, timeout=300)
        self.assertEqual((receipt["manifest_digest"], receipt["bytes"]), (digest, str(size)))
        self.assertEqual(checksum(output / "blob-0"), expected)
        n.stop()
        verify = [n.binaries / "psiosd", "verify-backup", "--directory", output, "--manifest-digest", digest]
        self.command(verify, error="QuotaExceeded")
        self.assertEqual(self.command(verify + ["--max-bytes", str(size)])["bytes"], str(size))

        receiver = Node(n.binaries)
        self.addCleanup(receiver.close)
        self.configure(receiver, size, restart=False)
        receipt = self.command(self.import_command(receiver, output, digest))
        self.configure(receiver, 256, restart=True)
        restored = receiver.perform("backup.restore", target(receipt["reference"]))
        tree = receiver.state / "volumes" / restored["resource_id"] / "data" / "payload"
        self.assertEqual((tree.stat().st_size, checksum(tree)), (size, expected))
        receiver.perform("volume.activate", target(restored))
