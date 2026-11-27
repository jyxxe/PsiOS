"""Exercise backed-up data-root permissions with real directories and Core processes."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import unittest

from support import Node, target


ROOT_MODE = 0o500
ROOT_MTIME_NS = 1_700_000_153_123_456_789
CONTENTS = "kept through recovery\n"
MODE_FIELD = "root_mode_unix_v1"


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


class BackupRootModeTests(unittest.TestCase):
    def node(self, binaries=None):
        node = Node(binaries or os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        return node

    def clean_mode(self, path):
        # A private directory can be read-only even for its owning test process.
        if path.is_dir():
            path.chmod(0o700)

    def data_path(self, node, reference):
        path = node.state / "volumes" / reference["resource_id"] / "data"
        self.addCleanup(self.clean_mode, path)
        return path

    def backup(self, node, source):
        reference = node.perform("backup.create", target(source))
        directory = node.state / "backups" / reference["resource_id"]
        manifest = (directory / "manifest.json").read_bytes()
        self.assertEqual(node.get(reference["resource_id"])["value"]["manifest_digest"], digest(manifest))
        return reference, directory, manifest

    def source(self, node):
        volume = node.perform("volume.create", {"name": "restricted-data-root"})
        volume = node.perform("volume.write", dict(target(volume), name="record", content=CONTENTS))["volume_ref"]
        root = self.data_path(node, volume)
        root.chmod(ROOT_MODE)
        os.utime(root, ns=(ROOT_MTIME_NS, ROOT_MTIME_NS))
        return volume, root

    def check_tree(self, root, mode):
        self.assertEqual(stat.S_IMODE(root.stat().st_mode), mode)
        self.assertEqual(root.stat().st_mtime_ns, ROOT_MTIME_NS)
        self.assertEqual((root / "record").read_text(), CONTENTS)

    def command(self, argv, error=None):
        result = subprocess.run(argv, capture_output=True, text=True, timeout=25)
        if error is not None:
            self.assertNotEqual(result.returncode, 0, result)
            self.assertIn(error, result.stderr + result.stdout)
        else:
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(result.stdout)

    def extract(self, daemon, source, manifest, output, error=None):
        self.addCleanup(self.clean_mode, output)
        return self.command([daemon, "verify-backup", "--directory", source,
                             "--manifest-digest", digest(manifest), "--restore-to", output], error)

    def test_restrictive_root_survives_restore_and_fences_activation_on_mode_drift(self):
        node = self.node()
        source, source_root = self.source(node)
        backup, _, manifest = self.backup(node, source)
        self.assertEqual(json.loads(manifest)[MODE_FIELD], ROOT_MODE)

        restored = node.perform("backup.restore", target(backup))
        root = self.data_path(node, restored)
        self.check_tree(root, ROOT_MODE)
        self.assertNotEqual(root.stat().st_ino, source_root.stat().st_ino)
        proof = node.get(restored["resource_id"])["resource"]["spec"]["restore_proof"]
        self.assertEqual(proof[MODE_FIELD], ROOT_MODE)
        self.assertTrue(proof["includes_mtime"])

        node.stop(signal.SIGKILL)
        root.chmod(0o700)
        self.assertEqual(root.stat().st_mtime_ns, ROOT_MTIME_NS)
        node.start()
        failed = node.wait(node.submit("volume.activate", target(restored)), autoapprove=True)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "Known"), failed)
        self.assertEqual(failed["conditions"][0]["type"], "PreconditionFailed", failed)
        self.assertIn("data root mode", failed["conditions"][0]["message"])
        pending = node.get(restored["resource_id"])
        self.assertEqual(pending["reference"], restored)
        self.assertEqual(pending["resource"]["spec"]["storage_restore_phase"], "AwaitingActivation")

        root.chmod(ROOT_MODE)
        activated = node.perform("volume.activate", target(restored))
        self.assertEqual(node.perform("volume.read", dict(target(activated), name="record"))["content"], CONTENTS)
        self.check_tree(root, ROOT_MODE)
        self.check_tree(source_root, ROOT_MODE)

    def test_backup_preflight_and_activation_reject_unsupported_root_metadata(self):
        node = self.node()
        source, root = self.source(node)
        root.chmod(0o750)
        backups = node.state / "backups"
        before = set(backups.iterdir())
        key = "user.psios-root-guard"
        payload = b"root metadata must be represented"
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 4, "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "backup.estimate"], "tool_targets": [source["resource_id"]]})

        def refused(task_id, code):
            task = node.wait(task_id, autoapprove=True)
            self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]), ("Failed", code), task)
            self.assertIn("extended attributes", task["conditions"][0]["message"])
            self.assertEqual(task["result"]["artifact_refs"], [])

        os.setxattr(root, key, payload)
        prepared = node.prepare("backup.estimate", target(source),
                                delegation=grant["delegation_ref"]["resource_id"])
        refused(node.call("Task.Submit", prepared)["task_id"], "UnsupportedCapability")
        refused(node.submit("backup.create", target(source)), "UnsupportedCapability")
        self.assertEqual(os.getxattr(root, key), payload)
        self.assertEqual(set(backups.iterdir()), before)
        self.assertEqual(node.call("Resource.List", {"kind": "Backup"})["items"], [])

        os.removexattr(root, key)
        root.chmod(0o2750)
        self.assertEqual(stat.S_IMODE(root.stat().st_mode), 0o2750)
        refused(node.submit("backup.create", target(source)), "UnsupportedCapability")
        root.chmod(0o750)
        backup, _, manifest = self.backup(node, source)
        self.assertEqual(json.loads(manifest)[MODE_FIELD], 0o750)

        restored = node.perform("backup.restore", target(backup))
        destination = self.data_path(node, restored)
        self.check_tree(destination, 0o750)
        os.setxattr(destination, key, payload)
        refused(node.submit("volume.activate", target(restored)), "PreconditionFailed")
        self.assertEqual(os.getxattr(destination, key), payload)
        self.assertEqual(node.get(restored["resource_id"])["reference"], restored)
        os.removexattr(destination, key)
        active = node.perform("volume.activate", target(restored))
        self.assertEqual(node.perform("volume.read", dict(target(active), name="record"))["content"], CONTENTS)

    def test_detached_extraction_applies_exact_mode_and_rejects_special_bits(self):
        node = self.node()
        source, _ = self.source(node)
        _, directory, manifest = self.backup(node, source)
        detached = node.base / "backup-copy"
        shutil.copytree(directory, detached)
        node.stop()
        node.state.rename(node.base / "offline-authority")
        self.addCleanup(self.clean_mode, node.base / "offline-authority" / "volumes" / source["resource_id"] / "data")
        node.witness.rename(node.base / "offline-witness")
        node.config.unlink()

        output = node.base / "detached-data"
        report = self.extract(node.daemon[0], detached, manifest, output)
        self.assertEqual(report["restore_proof"][MODE_FIELD], ROOT_MODE)
        self.check_tree(output, ROOT_MODE)

        altered = dict(json.loads(manifest), **{MODE_FIELD: 0o4500})
        altered_manifest = json.dumps(altered).encode()
        (detached / "manifest.json").write_bytes(altered_manifest)
        rejected = node.base / "unpublished"
        self.extract(node.daemon[0], detached, altered_manifest, rejected, "PreconditionFailed")
        self.assertFalse(rejected.exists())
        self.assertEqual(list(node.base.glob(".stage-*")), [])

    def test_managed_restore_requires_a_searchable_data_root_without_blocking_detached_extraction(self):
        source = self.node()
        volume, _ = self.source(source)
        _, directory, manifest = self.backup(source, volume)
        detached = source.base / "non-searchable-backup"
        shutil.copytree(directory, detached)
        # Detached input may originate from another producer using the same archive format.
        manifest = json.dumps(dict(json.loads(manifest), **{MODE_FIELD: 0o400})).encode()
        (detached / "manifest.json").write_bytes(manifest)
        output = source.base / "non-searchable-output"
        self.extract(source.daemon[0], detached, manifest, output)
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o400)

        receiver = self.node()
        receiver.stop()
        receipt = self.command(receiver.daemon + ["import-backup", "--directory", detached,
                                                  "--manifest-digest", digest(manifest)])
        receiver.start()
        failed = receiver.wait(receiver.submit("backup.restore", target(receipt["reference"])), autoapprove=True)
        self.assertEqual((failed["lifecycle"], failed["conditions"][0]["type"]),
                         ("Failed", "UnsupportedCapability"), failed)
        self.assertIn("owner read and search", failed["conditions"][0]["message"])
        self.assertEqual(list((receiver.state / "volumes").iterdir()), [])

    @unittest.skipUnless(os.getenv("PSIOS_BACKUP_ROOT_MODE_PRIOR_BIN_DIR"), "requires a genuine prior Core binary")
    def test_legacy_proof_survives_migration_and_previous_writer_is_fenced(self):
        node = self.node(os.environ["PSIOS_BACKUP_ROOT_MODE_PRIOR_BIN_DIR"])
        prior = node.daemon.copy()
        source, source_root = self.source(node)
        backup, _, manifest = self.backup(node, source)
        self.assertNotIn(MODE_FIELD, json.loads(manifest))
        prior_clone = node.perform("backup.restore", target(backup))
        self.check_tree(self.data_path(node, prior_clone), 0o700)
        prior_proof = node.get(prior_clone["resource_id"])["resource"]["spec"]["restore_proof"]
        self.assertNotIn(MODE_FIELD, prior_proof)
        node.stop()
        prior_format = self.command(prior + ["verify"])["database_format"]
        self.assertLess(prior_format, 112)

        # Open the same authority under the new writer without changing its stored configuration.
        node.daemon[0] = str(Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).absolute() / "psiosd")
        preview = self.command(node.daemon + ["verify", "--rehearse-migration"])
        self.assertEqual(preview["database_format"], prior_format)
        self.assertGreaterEqual(preview["migration_rehearsal"]["target_format"], 112)
        node.start()
        node.perform("volume.activate", target(prior_clone))
        restored = node.perform("backup.restore", target(backup))
        self.assertEqual(node.get(restored["resource_id"])["resource"]["spec"]["restore_proof"], prior_proof)
        self.check_tree(self.data_path(node, restored), 0o700)
        node.perform("volume.activate", target(restored))

        _, directory, manifest = self.backup(node, source)
        self.assertEqual(json.loads(manifest)[MODE_FIELD], ROOT_MODE)
        self.check_tree(source_root, ROOT_MODE)
        node.stop()
        self.command(prior + ["verify"], "SchemaVersionUnsupported")
        self.command(prior + ["serve"], "SchemaVersionUnsupported")

        legacy_output = node.base / "legacy-extraction"
        current_output = node.base / "current-extraction"
        self.extract(prior[0], directory, manifest, legacy_output)
        self.extract(node.daemon[0], directory, manifest, current_output)
        self.check_tree(legacy_output, 0o700)
        self.check_tree(current_output, ROOT_MODE)
        summary = self.command(node.daemon + ["verify"])
        self.assertGreaterEqual(summary["database_format"], 112)
