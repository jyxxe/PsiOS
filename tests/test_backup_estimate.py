"""Measure the exact backup source profile through normal scoped Task admission."""

import json
import os
from pathlib import Path
import signal
import subprocess
import unittest

from support import Failure, Node, target
from test_artifact_install import grant


TOOL = "backup.estimate"


class BackupEstimateTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "backup-estimate"})
        self.source = self.n.state / "volumes" / self.volume["resource_id"] / "data"

    def prepared(self, reference=None):
        reference = reference or self.volume
        allowed = grant(self.n, "local-owner", ["task.submit", "task.read", TOOL], [reference["resource_id"]])
        return self.n.prepare(TOOL, target(reference), delegation=allowed["delegation_ref"]["resource_id"])

    def submit(self, reference=None):
        return self.n.call("Task.Submit", self.prepared(reference))["task_id"]

    def estimate(self, reference=None):
        task = self.n.wait(self.submit(reference))
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        return self.n.call("Artifact.Read", {"reference": artifact})

    def require_failed(self, task_id, code):
        completed = self.n.wait(task_id, autoapprove=True)
        self.assertEqual((completed["lifecycle"], completed["conditions"][0]["type"]), ("Failed", code), completed)
        self.assertEqual(completed["result"]["artifact_refs"], [])

    def test_entry_limit_allows_large_capture_and_restore_after_a_lower_limit(self):
        n = self.n
        directory = self.source / "records"
        directory.mkdir()
        for index in range(4100):
            (directory / f"record-{index:04}").touch()
        marker = directory / 'escaped "name" \\ evidence'
        marker.write_bytes(b"verified payload")
        entries = 4102

        default = self.estimate()
        self.assertEqual((default["entries"], default["profile"]["entry_limit"]), (entries, 4096))
        self.assertFalse(default["fits_entry_limit"])
        self.require_failed(n.submit("backup.create", target(self.volume)), "QuotaExceeded")
        self.assertEqual(list((n.state / "backups").iterdir()), [])

        n.stop()
        original = n.config.read_text()
        config = json.loads(original)
        for invalid in (0, 16385):
            n.config.write_text(json.dumps(dict(config, backup_max_entries=invalid)))
            refused = subprocess.run(n.daemon + ["check"], capture_output=True, text=True, timeout=15)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn("InvalidArgument", refused.stderr)
        n.config.write_text(json.dumps(dict(config, backup_max_entries=entries)))
        n.start()
        self.assertEqual(n.call("StoragePool.Get", {"id": "local"})["backup_max_entries"], entries)
        self.assertTrue(self.estimate()["fits_entry_limit"])
        backup = n.perform("backup.create", target(self.volume))
        receipt = n.get(backup["resource_id"])["value"]
        archive = n.state / "backups" / backup["resource_id"]
        self.assertEqual((receipt["entries"], receipt["bytes"]), (entries, str(marker.stat().st_size)))
        verifier = [n.binaries / "psiosd", "verify-backup", "--directory", archive,
                    "--manifest-digest", receipt["manifest_digest"]]
        verified = json.loads(subprocess.check_output(verifier, timeout=30))
        self.assertEqual(verified["entries"], entries)

        n.stop(signal.SIGKILL)
        n.config.write_text(original)
        n.start()
        restored = n.perform("backup.restore", target(backup))
        restored = n.perform("volume.activate", target(restored))
        tree = n.state / "volumes" / restored["resource_id"] / "data" / "records"
        self.assertEqual(len(list(tree.iterdir())), entries - 1)
        self.assertEqual((tree / marker.name).read_bytes(), marker.read_bytes())
        self.assertEqual((tree / marker.name).stat().st_mtime_ns, marker.stat().st_mtime_ns)

        receiver = Node(n.binaries)
        self.addCleanup(receiver.close)
        receiver.stop()
        receiver_config = receiver.config.read_text()
        intake = receiver.daemon + ["import-backup", "--directory", archive,
                                     "--manifest-digest", receipt["manifest_digest"]]
        refused = subprocess.run(intake, capture_output=True, text=True, timeout=30)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("QuotaExceeded", refused.stderr)
        self.assertEqual(list((receiver.state / "backups").iterdir()), [])
        receiver.config.write_text(json.dumps(dict(json.loads(receiver_config), backup_max_entries=entries)))
        imported = json.loads(subprocess.check_output(intake, timeout=45))
        self.assertEqual(imported["manifest_digest"], receipt["manifest_digest"])
        receiver.config.write_text(receiver_config)
        self.assertEqual(json.loads(subprocess.check_output(intake, timeout=45)), dict(imported, reused=True))

    @unittest.skipUnless(os.getenv("PSIOS_BACKUP_ENTRIES_PREVIOUS_BIN_DIR"), "requires the format-133 Core")
    def test_existing_fixed_limit_then_upgrade_fences_the_prior_writer(self):
        previous = Path(os.environ["PSIOS_BACKUP_ENTRIES_PREVIOUS_BIN_DIR"])
        node = Node(previous)
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "old-fixed-entry-limit"})
        source = node.state / "volumes" / volume["resource_id"] / "data"
        for index in range(4097):
            (source / str(index)).touch()
        old = node.wait(node.submit(TOOL, target(volume)), autoapprove=True)
        self.assertEqual((old["lifecycle"], old["conditions"][0]["type"]), ("Failed", "QuotaExceeded"), old)
        node.stop(signal.SIGKILL)
        old_daemon = node.daemon[:]
        prior = json.loads(subprocess.check_output(old_daemon + ["verify"], timeout=20))
        self.assertEqual((prior["database_format"], prior["migration_required"]), (133, False))

        node.binaries = self.n.binaries
        node.daemon[0] = str(node.binaries / "psiosd")
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        self.assertEqual((before["node_id"], before["database_format"], before["migration_required"]),
                         (prior["node_id"], 133, True))
        node.start()
        estimate = node.perform(TOOL, target(volume))
        self.assertEqual((estimate["entries"], estimate["fits_entry_limit"]), (4097, False))
        self.assertEqual(node.call("Task.Get", {"id": old["task_id"]}), old)
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        self.assertEqual((after["node_id"], after["database_format"], after["migration_required"]),
                         (prior["node_id"], 134, False))
        fenced = subprocess.run(old_daemon + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(fenced.returncode, 0)
        self.assertIn("SchemaVersionUnsupported", fenced.stderr)

    def test_sparse_payload_preflight_uses_the_same_basis_as_capture(self):
        n = self.n
        n.stop()
        config = json.loads(n.config.read_text())
        config["backup_max_bytes"] = 65536
        n.config.write_text(json.dumps(config))
        n.start()
        (self.source / "nested").mkdir()
        payload = self.source / "nested" / "sparse"
        with payload.open("xb") as file:
            file.truncate(4 * 1024 * 1024 * 1024)
        (self.source / "note").write_bytes(b"note")
        os.utime(payload, ns=(0, payload.stat().st_mtime_ns))
        before = payload.stat()
        backups = n.state / "backups"
        initial = set(backups.iterdir())
        result = self.estimate()
        self.assertEqual(result["source_ref"], self.volume)
        self.assertEqual((result["files"], result["directories"], result["entries"]), (2, 1, 3))
        self.assertEqual(result["logical_bytes"], str(before.st_size + 4))
        self.assertFalse(result["fits_payload_limit"])
        self.assertTrue(result["fits_entry_limit"])
        self.assertEqual(result["profile"], {"payload_limit_bytes": "65536", "entry_limit": 4096,
                         "entry_hard_limit": 16384, "directory_depth_limit": 32, "capture_timeout_seconds": 30})
        self.assertEqual(payload.stat().st_atime_ns, before.st_atime_ns)
        self.assertEqual(set(backups.iterdir()), initial)
        self.require_failed(n.submit("backup.create", target(self.volume)), "QuotaExceeded")
        self.assertEqual(set(backups.iterdir()), initial)

        with payload.open("r+b") as file:
            file.truncate(4096)
        fit = self.estimate()
        self.assertTrue(fit["fits_payload_limit"])
        saved = n.perform("backup.create", target(self.volume))
        manifest = json.loads((backups / saved["resource_id"] / "manifest.json").read_text())
        self.assertEqual((fit["logical_bytes"], fit["entries"]), (manifest["total_bytes"], len(manifest["entries"])))
        self.assertEqual(fit["data_incarnation"], manifest["data_incarnation"])

    def test_scope_unsafe_source_and_restored_activation_share_storage_rules(self):
        n = self.n
        other = n.perform("volume.create", {"name": "different-scope"})
        with self.assertRaises(Failure) as denied:
            wrong = grant(n, "local-owner", ["task.submit", "task.read", TOOL], [other["resource_id"]])
            prepare = n.prepare(TOOL, target(self.volume), delegation=wrong["delegation_ref"]["resource_id"])
            n.call("Task.Submit", prepare)
        self.assertEqual(denied.exception.code, "PermissionDenied", denied.exception)
        (self.source / "unsafe").symlink_to("/dev/null")
        self.require_failed(self.submit(), "UnsupportedCapability")
        self.require_failed(n.submit("backup.create", target(self.volume)), "UnsupportedCapability")
        (self.source / "unsafe").unlink()

        backup = n.perform("backup.create", target(self.volume))
        restored = n.perform("backup.restore", target(backup))
        self.require_failed(self.submit(restored), "PreconditionFailed")
        restored = n.perform("volume.activate", target(restored))
        prepared = self.prepared(restored)
        settled = n.call("Task.Submit", prepared)["task_id"]
        state = n.wait(settled)
        self.assertEqual(state["lifecycle"], "Succeeded", state)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Task.Get", {"id": settled}), state)
        repeated = n.call("Task.Submit", prepared)
        self.assertEqual(repeated["task_id"], settled)
        self.assertTrue(repeated["deduplicated"])
