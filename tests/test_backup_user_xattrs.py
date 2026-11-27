"""Keep real Linux user attributes through backup, detached restore, and clone activation."""
import errno
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import unittest

from support import Node, target
from test_artifact_install import grant


class BackupUserAttributesTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.volume = self.node.perform("volume.create", {"name": "attribute-capture-source"})
        self.source = self.data(self.volume)
        self.config = json.loads(self.node.config.read_text())
        self.expected = {".": {"user.label": b"original root", "user.empty": b""},
                         "nested": {"user.kind": b"directory"},
                         "nested/payload": {"user.binary": b"\x00\xff\x00\n"}}
        (self.source / "nested").mkdir()
        (self.source / "nested/payload").write_bytes(b"persistent application data")
        for suffix, attributes in self.expected.items():
            for name, value in attributes.items():
                try:
                    os.setxattr(self.source / suffix, name, value)
                except OSError as error:
                    if error.errno == errno.EOPNOTSUPP:
                        self.skipTest("fixture filesystem lacks Linux user attributes")
                    raise

    def data(self, volume):
        return self.node.state / "volumes" / volume["resource_id"] / "data"

    def configure(self, preserve):
        self.node.stop()
        self.node.config.write_text(json.dumps(dict(self.config, backup_user_xattrs=preserve)))
        self.node.start()

    def attributes(self, root):
        return {path: {name: os.getxattr(root / path, name) for name in os.listxattr(root / path)}
                for path in self.expected}

    def failure(self, tool, parameters, expected):
        completed = self.node.wait(self.node.submit(tool, parameters), autoapprove=True)
        self.assertEqual((completed["lifecycle"], completed["conditions"][0]["type"]), ("Failed", expected), completed)
        self.assertEqual(completed["result"]["artifact_refs"], [])
        return completed

    def archive(self, backup):
        directory = self.node.state / "backups" / backup["resource_id"]
        digest = self.node.get(backup["resource_id"])["value"]["manifest_digest"]
        return directory, digest

    def test_profile_preserves_root_directory_and_file_attributes_after_restart(self):
        n = self.node
        self.failure("backup.create", target(self.volume), "UnsupportedCapability")
        self.configure(True)
        self.assertTrue(n.call("StoragePool.Get", {"id": "local"})["backup_user_xattrs"])
        allowed = grant(n, "local-owner", ["task.submit", "task.read", "backup.estimate"], [self.volume["resource_id"]])
        request = n.prepare("backup.estimate", target(self.volume), delegation=allowed["delegation_ref"]["resource_id"])
        estimation = n.wait(n.call("Task.Submit", request)["task_id"])
        self.assertEqual(estimation["lifecycle"], "Succeeded", estimation)
        scan = n.call("Artifact.Read", {"reference": estimation["result"]["artifact_refs"][0]})
        self.assertGreater(scan["user_xattr_metadata_bytes"], 64)
        saved = n.perform("backup.create", target(self.volume))
        archive, digest = self.archive(saved)
        payload = json.loads((archive / "manifest.json").read_text())
        self.assertNotIn("entries", payload)
        self.assertEqual(len(payload["entries_with_user_xattrs"]), 2)
        self.assertIn("user_xattrs_unix_v1", payload)
        offline = n.base / "detached-restore"
        command = [n.binaries / "psiosd", "verify-backup", "--directory", archive, "--manifest-digest", digest]
        subprocess.run(command + ["--restore-to", offline], check=True, capture_output=True, timeout=20)
        self.assertEqual(self.attributes(offline), self.expected)

        allowed = grant(n, "local-owner", ["task.submit", "task.read", "volume.clone"], [self.volume["resource_id"], "local"])
        request = n.prepare("volume.clone", {"target": target(self.volume), "name": "attribute-clone"},
                            delegation=allowed["delegation_ref"]["resource_id"])
        completed = n.wait(n.call("Task.Submit", request)["task_id"], autoapprove=True)
        self.assertEqual(completed["lifecycle"], "Succeeded", completed)
        clone = n.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
        self.assertEqual(self.attributes(self.data(clone)), self.expected)
        n.stop(signal.SIGKILL)
        self.configure(False)
        self.assertEqual(self.attributes(self.data(n.perform("volume.activate", target(clone)))), self.expected)

        restored = n.perform("backup.restore", target(saved))
        tree = self.data(restored)
        self.assertEqual(self.attributes(tree), self.expected)
        os.setxattr(tree / "nested/payload", "user.binary", b"changed")
        refused = self.failure("volume.activate", target(restored), "PreconditionFailed")
        self.assertIn("user attributes", refused["conditions"][0]["message"])
        os.setxattr(tree / "nested/payload", "user.binary", self.expected["nested/payload"]["user.binary"])
        n.perform("volume.activate", target(restored))
        self.assertEqual(self.attributes(tree), self.expected)
        with (tree / "nested/payload").open("rb") as file:
            self.assertEqual(file.read(), b"persistent application data")

        if previous := os.getenv("PSIOS_USER_XATTR_PREVIOUS_BIN_DIR"):
            refused = subprocess.run([Path(previous) / "psiosd", *command[1:], "--restore-to", n.base / "older-copy"],
                                     capture_output=True, text=True, timeout=20)
            self.assertNotEqual(refused.returncode, 0, refused)
            self.assertFalse((n.base / "older-copy").exists())
            self.assertIn("missing field `entries`", refused.stderr)
        n.stop()
        result = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertEqual((result["database_format"], result["sqlite_integrity"]), (137, "Verified"))

    def test_attribute_count_and_foreign_names_are_refused_before_publication(self):
        n = self.node
        self.configure(True)
        for index in range(15):
            os.setxattr(self.source, f"user.extra{index:02}", b"x")
        self.failure("backup.create", target(self.volume), "QuotaExceeded")
        self.assertEqual(list((n.state / "backups").iterdir()), [])
        os.removexattr(self.source, "user.extra14")
        saved = n.perform("backup.create", target(self.volume))
        archive, _ = self.archive(saved)
        external = n.base / "foreign-archive"
        shutil.copytree(archive, external)
        manifest = external / "manifest.json"
        payload = json.loads(manifest.read_text())
        payload["user_xattrs_unix_v1"]["root"] = [{"name":"security.capability","value_base64":"AA=="}]
        raw = json.dumps(payload).encode()
        manifest.write_bytes(raw)
        digest = "sha256:" + hashlib.sha256(raw).hexdigest()
        output = n.base / "foreign-output"
        refused = subprocess.run([n.binaries / "psiosd", "verify-backup", "--directory", external,
                                  "--manifest-digest", digest, "--restore-to", output], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(refused.returncode, 0, refused)
        self.assertIn("invalid backup user attribute name", refused.stderr)
        self.assertFalse(output.exists())
