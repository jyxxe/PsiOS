"""Read real volume files through the CLI and the audited Core Tasks."""
from concurrent.futures import ThreadPoolExecutor
import json
import socket
import subprocess
import unittest

from model_delivery import frame
from support import Node, target


class VolumeReadCli(unittest.TestCase):
    def setUp(self):
        self.node = Node()
        self.addCleanup(self.node.close)
        self.volume = self.node.perform("volume.create", {"name": "file-read-cli"})
        self.data = self.node.state / "volumes" / self.volume["resource_id"] / "data"
        self.grant = self.node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", "volume.read_chunk"],
            "tool_targets": [self.volume["resource_id"]], "units": 16, "ttl_seconds": 120,
        })

    def read(self, path="payload", *, volume=None, version=None, limit=384 * 1024, address=None):
        return subprocess.run([
            self.node.binaries / "psiosctl", "--socket", address or self.node.socket,
            "--token-file", self.node.witness / "owner.token", "volume-read", "--volume",
            volume or self.volume["resource_id"], "--revision", version or self.volume["version"],
            "--delegation", self.grant["delegation_ref"]["resource_id"], "--max-bytes", str(limit), path,
        ], capture_output=True, timeout=60)

    def expect_refusal(self, result, *codes):
        self.assertNotEqual(result.returncode, 0, result)
        self.assertEqual(result.stdout, b"")
        if codes:
            error = result.stderr.decode().split(":", 1)[0]
            self.assertIn(error, codes, result.stderr)

    def test_verified_binary_bytes_and_empty_file(self):
        payload = bytes(range(256)) * 511
        (self.data / "payload").write_bytes(payload)
        read = self.read(limit=len(payload))
        self.assertEqual(read.returncode, 0, read.stderr)
        self.assertEqual(read.stdout, payload)
        self.assertEqual(read.stderr, b"")
        (self.data / "empty").touch()
        empty = self.read("empty", limit=1)
        self.assertEqual((empty.returncode, empty.stdout, empty.stderr), (0, b"", b""))
        self.expect_refusal(self.read(limit=len(payload) - 1), "QuotaExceeded")
        self.expect_refusal(self.read(limit=384 * 1024 + 1), "InvalidArgument")

    def test_scope_revision_and_path_refusals(self):
        (self.data / "payload").write_bytes(b"expected")
        outside = self.node.base / "external"
        outside.write_bytes(b"unrelated file")
        (self.data / "link").symlink_to(outside)
        self.expect_refusal(self.read("../external"), "InvalidArgument")
        self.expect_refusal(self.read("link"))
        self.expect_refusal(self.read(version="r9999"), "Conflict", "PreconditionFailed")
        other = self.node.perform("volume.create", {"name": "other-authorization-scope"})
        self.expect_refusal(self.read(volume=other["resource_id"], version=other["version"]), "PermissionDenied")
        self.node.call("Grant.Revoke", target(self.grant["grant_ref"]))
        self.expect_refusal(self.read(), "PermissionDenied")

    def read_with_change_between_chunks(self, change):
        proxy = self.node.base / "read-gate.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(1) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(0.1)
            reading = pool.submit(self.read, address=proxy)
            revision = None
            while not reading.done():
                try:
                    peer, _ = listener.accept()
                except TimeoutError:
                    continue
                with peer, socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream:
                    peer.settimeout(8)
                    upstream.settimeout(8)
                    raw = frame(peer)
                    request = json.loads(raw[4:])
                    if request["method"] == "Task.Prepare" and request["params"]["input"]["offset"] > 0:
                        self.assertIsNone(revision)
                        details = request["params"]["input"]
                        self.assertEqual(details["offset"], 96 * 1024)
                        revision = details["expected_file_revision"]
                        change()
                    upstream.connect(str(self.node.socket))
                    upstream.sendall(raw)
                    peer.sendall(frame(upstream))
            self.assertRegex(revision, r"^sha256:[0-9a-f]{64}$")
            return reading.result()

    def test_same_size_replacement_mid_read_releases_no_bytes(self):
        payload = self.data / "payload"
        replacement = self.data / "replacement"
        payload.write_bytes(b"a" * (120 * 1024))
        replacement.write_bytes(b"b" * payload.stat().st_size)
        original_inode = payload.stat().st_ino
        result = self.read_with_change_between_chunks(lambda: replacement.replace(payload))
        self.expect_refusal(result, "Conflict")
        self.assertNotEqual(payload.stat().st_ino, original_inode)
        self.assertEqual(self.read().stdout, b"b" * (120 * 1024))

    def test_revocation_between_chunks_releases_no_bytes(self):
        (self.data / "payload").write_bytes(b"private\x00" * (15 * 1024))
        result = self.read_with_change_between_chunks(
            lambda: self.node.call("Grant.Revoke", target(self.grant["grant_ref"]))
        )
        self.expect_refusal(result, "PermissionDenied")
