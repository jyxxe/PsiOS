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
        self.expected = {
            ".": {"user.label": b"original root", "user.empty": b""},
            "nested": {"user.kind": b"directory"},
            "nested/payload": {"user.binary": b"\x00\xff\x00\n"},
        }
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

    def export(self, reference, output):
        n = self.node
        agent = n.call("Agent.Register", {"name": "attribute-backup-exporter"})
        grant(n, agent["principal_id"], ["backup.export"], [reference["resource_id"]])
        token = n.base / "backup-export.token"
        token.touch(mode=0o600)
        token.write_text(agent["token"])
        result = subprocess.run(
            [n.binaries / "psiosctl", "--socket", n.socket, "--token-file", token,
             "backup-export", "--output", output],
            input=json.dumps(reference), capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

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
            self.assertIn('code: "InvalidArgument"', refused.stderr)
        n.stop()
        result = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(result["database_format"], 137)
        self.assertEqual(result["sqlite_integrity"], "Verified")

    def test_export_and_import_on_a_different_node(self):
        self.configure(True)
        saved = self.node.perform("backup.create", target(self.volume))
        archive, digest = self.archive(saved)
        exported = self.node.base / "exported"
        self.assertEqual(self.export(saved, exported)["manifest_digest"], digest)
        self.assertEqual({path.name: path.read_bytes() for path in exported.iterdir()},
                         {path.name: path.read_bytes() for path in archive.iterdir()})
        receiver = Node(self.node.binaries)
        self.addCleanup(receiver.close)
        self.assertNotEqual(self.node.call("Node.Get", {})["reference"]["resource_id"],
                            receiver.call("Node.Get", {})["reference"]["resource_id"])
        self.assertFalse(receiver.call("StoragePool.Get", {"id": "local"})["backup_user_xattrs"])
        receiver.stop()
        imported = subprocess.run(
            [*receiver.daemon, "import-backup", "--directory", exported, "--manifest-digest", digest],
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(imported.returncode, 0, imported.stderr)
        receiver.start()
        restored = receiver.perform("backup.restore", target(json.loads(imported.stdout)["reference"]))
        active = receiver.perform("volume.activate", target(restored))
        recovered = receiver.state / "volumes" / active["resource_id"] / "data"
        self.assertEqual(self.attributes(recovered), self.expected)
        self.assertEqual((recovered / "nested/payload").read_bytes(), b"persistent application data")

    @unittest.skipUnless(os.getenv("PSIOS_USER_XATTR_PREVIOUS_BIN_DIR"), "requires pinned format-136 binaries")
    def test_old_authority_upgrades_without_changing_an_existing_plain_backup(self):
        previous = Node(os.environ["PSIOS_USER_XATTR_PREVIOUS_BIN_DIR"])
        self.addCleanup(previous.close)
        original = previous.daemon[:]
        source = previous.perform("volume.create", {"name": "plain-format-136"})
        source = previous.perform("volume.write", {
            **target(source), "name": "record", "content": "data created by format 136",
        })["volume_ref"]
        saved = previous.perform("backup.create", target(source))
        document = previous.get(saved["resource_id"])
        previous.stop(signal.SIGKILL)
        before = json.loads(subprocess.check_output([*original, "verify"], timeout=20))
        self.assertEqual((before["database_format"], before["migration_required"]), (136, False))

        previous.binaries = self.node.binaries
        previous.daemon[0] = str(previous.binaries / "psiosd")
        planned = json.loads(subprocess.check_output([*previous.daemon, "verify"], timeout=20))
        self.assertEqual((planned["node_id"], planned["database_format"], planned["migration_required"]),
                         (before["node_id"], 136, True))
        previous.start()
        self.assertEqual(previous.get(saved["resource_id"]), document)
        restored = previous.perform("backup.restore", target(saved))
        active = previous.perform("volume.activate", target(restored))
        record = previous.state / "volumes" / active["resource_id"] / "data" / "record"
        self.assertEqual(record.read_text(), "data created by format 136")
        self.assertEqual(os.listxattr(record), [])
        previous.stop()
        after = json.loads(subprocess.check_output([*previous.daemon, "verify"], timeout=20))
        self.assertEqual(after["node_id"], before["node_id"])
        self.assertGreaterEqual(after["database_format"], 137)
        self.assertFalse(after["migration_required"])
        refused = subprocess.run([*original, "serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(refused.returncode, 0, refused)
        self.assertIn("SchemaVersionUnsupported", refused.stderr)

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
        payload["user_xattrs_unix_v1"]["root"] = [
            {"name": "security.capability", "value_base64": "AA=="},
        ]
        raw = json.dumps(payload).encode()
        manifest.write_bytes(raw)
        digest = "sha256:" + hashlib.sha256(raw).hexdigest()
        output = n.base / "foreign-output"
        refused = subprocess.run([n.binaries / "psiosd", "verify-backup", "--directory", external,
                                  "--manifest-digest", digest, "--restore-to", output], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(refused.returncode, 0, refused)
        self.assertIn("invalid backup user attribute name", refused.stderr)
        self.assertFalse(output.exists())
