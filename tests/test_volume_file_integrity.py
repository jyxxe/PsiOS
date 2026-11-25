"""Hash real volume file bytes through the Task broker with authorization and identity checks."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest

from support import Failure, Node, target


def digest(payload):
    return "sha256:" + hashlib.sha256(payload).hexdigest()


class FileIntegrityTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "pinned-file"})
        self.data = self.n.state / "volumes" / self.volume["resource_id"] / "data"
        (self.data / "nested").mkdir()
        self.file = self.data / "nested" / "payload"
        self.grant = self.issue(self.volume)

    def issue(self, volume):
        return self.n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", "volume.verify_file"],
            "tool_targets": [volume["resource_id"]], "units": 12, "ttl_seconds": 180,
        })

    def input(self, volume=None, **options):
        return {"target": target(volume or self.volume), "path": "nested/payload", **options}

    def submit(self, data=None, grant=None):
        n = self.n
        request = n.prepare("volume.verify_file", data or self.input(),
                            delegation=(grant or self.grant)["delegation_ref"]["resource_id"])
        return n.call("Task.Submit", request)["task_id"]

    def finish(self, task):
        result = self.n.wait(task)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Succeeded", "Known"), result)
        return self.n.call("Artifact.Read", {"reference": result["result"]["artifact_refs"][0]})

    def assert_failed(self, task, *codes):
        result = self.n.wait(task)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Failed", "Known"), result)
        self.assertIn(result["conditions"][0]["type"], codes, result)
        self.assertFalse(result["result"]["artifact_refs"])

    def test_binary_digest_limit_and_restored_volume(self):
        payload = b"\x00\xffprivate\n" * 120000
        self.file.write_bytes(payload)
        expected = digest(payload)
        reply = self.finish(self.submit(self.input(expected_digest=expected, max_bytes=len(payload))))
        self.assertEqual(reply["content_digest"], expected)
        self.assertEqual(set(reply), {"volume_ref", "data_incarnation", "path", "revision",
                                      "content_digest", "observation_digest"})
        self.assertEqual(reply["volume_ref"], self.volume)
        self.assertEqual(reply["revision"]["size_bytes"], str(len(payload)))
        self.assertEqual(reply["revision"]["inode"], str(self.file.stat().st_ino))
        seal = reply.pop("observation_digest")
        self.assertEqual(seal, digest(b"psios/volume-file-observation/1\n" + json.dumps(
            reply, sort_keys=True, separators=(",", ":")
        ).encode()))
        self.assert_failed(self.submit(self.input(max_bytes=len(payload) - 1)), "QuotaExceeded")
        self.assert_failed(self.submit(self.input(expected_digest=digest(b"wrong"))), "PreconditionFailed")

        backup = self.n.perform("backup.create", target(self.volume))
        restored = self.n.perform("backup.restore", target(backup))
        spec = self.n.get(restored["resource_id"])["resource"]["spec"]
        self.assertEqual(spec["storage_restore_phase"], "AwaitingActivation")
        replay = self.finish(self.submit(self.input(restored, expected_digest=expected), self.issue(restored)))
        self.assertEqual(replay["content_digest"], expected)
        self.assertNotEqual(replay["data_incarnation"], reply["data_incarnation"])

    def test_scope_and_unsafe_entries(self):
        self.file.write_text("pinned")
        with self.assertRaises(Failure) as unscoped:
            self.n.prepare("volume.verify_file", self.input())
        self.assertEqual(unscoped.exception.code, "PermissionDenied")
        for invalid in ("../payload", "nested//payload"):
            with self.assertRaises(Failure) as refused:
                self.submit(self.input(path=invalid))
            self.assertEqual(refused.exception.code, "InvalidArgument")
        symlink = self.data / "nested" / "link"
        symlink.symlink_to(self.file)
        os.link(self.file, self.data / "nested" / "hard-link")
        fifo = self.data / "nested" / "fifo"
        os.mkfifo(fifo)
        for path in ("nested/link", "nested/hard-link", "nested/fifo", "nested"):
            with self.subTest(path=path):
                self.assert_failed(self.submit(self.input(path=path)), "UnsupportedCapability", "StorageUnavailable")
        unrelated = self.n.perform("volume.create", {"name": "not-granted"})
        with self.assertRaises(Failure) as refused:
            self.submit(self.input(unrelated))
        self.assertEqual(refused.exception.code, "PermissionDenied")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_BIND"), "mount namespace fixture")
    def test_mount_boundary_refuses_file_bind_from_same_filesystem(self):
        outside = self.n.base / "outside-volume"
        outside.write_bytes(b"external content")
        self.file.write_bytes(b"the volume file")
        self.assertEqual(outside.stat().st_dev, self.file.stat().st_dev)
        subprocess.run(["mount", "--bind", outside, self.file], check=True)
        try:
            self.assert_failed(self.submit(), "StorageUnavailable")
        finally:
            subprocess.run(["umount", self.file], check=True)
        self.assertEqual(self.finish(self.submit())["content_digest"], digest(b"the volume file"))

    def wait_open(self):
        proc = self.n.process
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            for fd in Path(f"/proc/{proc.pid}/fd").iterdir():
                try:
                    if os.readlink(fd) == str(self.file):
                        return
                except FileNotFoundError:
                    pass
            time.sleep(0.005)
        self.fail("no live file verification descriptor observed")

    def test_changed_inode_and_revoked_stream_are_never_published(self):
        n = self.n
        limit = 128 * 1024 * 1024
        with self.file.open("wb") as stream:
            stream.truncate(limit)
        active = self.submit(self.input(max_bytes=limit))
        self.wait_open()
        n.process.send_signal(signal.SIGSTOP)
        try:
            _, status = os.waitpid(n.process.pid, os.WUNTRACED)
            self.assertTrue(os.WIFSTOPPED(status))
            replacement = self.file.with_name("replacement")
            with replacement.open("wb") as stream:
                stream.truncate(limit)
            os.replace(replacement, self.file)
        finally:
            n.process.send_signal(signal.SIGCONT)
        self.assert_failed(active, "PreconditionFailed")
        active = self.submit(self.input(max_bytes=limit))
        self.wait_open()
        n.call("Grant.Revoke", target(self.grant["grant_ref"]))
        self.assert_failed(active, "Fenced", "PermissionDenied")
