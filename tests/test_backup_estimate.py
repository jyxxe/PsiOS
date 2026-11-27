"""Measure the exact backup source profile through normal scoped Task admission."""

import json
import os
import signal
import unittest

from support import Failure, Node, target
from test_artifact_install import grant


TOOL = "backup.estimate"


class BackupEstimateTests(unittest.TestCase):
    def setUp(self):
        self.n = Node()
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
        self.assertEqual(result["profile"], {"payload_limit_bytes": "65536", "entry_limit": 4096,
                         "directory_depth_limit": 32, "capture_timeout_seconds": 30})
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
