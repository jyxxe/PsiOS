"""Interrupt real backup captures before the irreversible publication checkpoint."""
import json
import os
import signal
import subprocess
import time
import unittest
from datetime import datetime, timedelta, timezone

from support import Node, target


class BackupCaptureControlTests(unittest.TestCase):
    size = 128 * 1024 * 1024

    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "capture-control"})
        self.source = self.n.state / "volumes" / self.volume["resource_id"] / "data" / "payload"
        with self.source.open("wb") as stream:
            stream.write(b"unchanged source")
            stream.truncate(self.size)
        self.backups = self.n.state / "backups"

    def configure(self, seconds, start=True):
        n = self.n
        n.stop()
        cfg = json.loads(n.config.read_text())
        cfg["backup_capture_timeout_seconds"] = seconds
        n.config.write_text(json.dumps(cfg))
        if start:
            n.start()
            self.assertEqual(n.call("StoragePool.Get", {"id": "local"})["backup_capture_timeout_seconds"], seconds)

    def observe_partial(self):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            stage = next(self.backups.glob(".stage-*/blob-0"), None)
            if stage is not None:
                try:
                    # The written prefix reaches staging before hashing the long trailing hole.
                    # That hole can stay unallocated until the final logical length is set.
                    if 0 < stage.stat().st_size < self.size:
                        return stage
                except FileNotFoundError:
                    pass
            time.sleep(0.005)
        self.fail("capture did not expose a partial file in private staging")

    def still_original(self):
        self.assertEqual(list(self.backups.iterdir()), [])
        self.assertEqual(self.source.stat().st_size, self.size)
        with self.source.open("rb") as stream:
            self.assertEqual(stream.read(16), b"unchanged source")
        self.assertEqual(self.n.get(self.volume["resource_id"])["reference"], self.volume)

    def permit(self, task):
        n = self.n
        t = n.call("Task.Get", {"id": task})
        op_id = t["operation_refs"][0]["resource_id"]
        op = n.call("Operation.Get", {"id": op_id})
        effect = n.get(op["effect_refs"][0]["resource_id"])["resource"]
        return n.get(effect["spec"]["permit_ref"]["resource_id"])["value"]

    def test_cancel_and_shutdown_stop_partial_copy_without_publishing(self):
        n = self.n
        task = n.submit("backup.create", target(self.volume))
        stage = self.observe_partial()
        n.call("Task.Cancel", target(n.get(task)["reference"]))
        cancelled = n.wait(task)
        self.assertEqual((cancelled["lifecycle"], cancelled["outcome_knowledge"]), ("Cancelled", "Known"), cancelled)
        self.assertFalse(stage.exists())
        self.still_original()

        interrupted = n.submit("backup.create", target(self.volume))
        stage = self.observe_partial()
        n.stop()
        self.assertFalse(stage.exists())
        n.start()
        failed = n.wait(interrupted)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "Known"), failed)
        self.assertEqual(failed["conditions"][0]["type"], "LeaseLost")
        self.still_original()

    def test_capture_deadline_expires_during_native_process_pause(self):
        n = self.n
        self.configure(1)
        task = n.submit("backup.create", target(self.volume))
        self.observe_partial()
        permit = self.permit(task)
        span = datetime.fromisoformat(permit["not_after"]) - datetime.fromisoformat(permit["issued_at"])
        self.assertGreater(span.total_seconds(), 0.8)
        self.assertLessEqual(span.total_seconds(), 1)
        n.process.send_signal(signal.SIGSTOP)
        try:
            _, wait_status = os.waitpid(n.process.pid, os.WUNTRACED)
            self.assertTrue(os.WIFSTOPPED(wait_status))
            time.sleep(1.15)
        finally:
            n.process.send_signal(signal.SIGCONT)
        expired = n.wait(task)
        self.assertEqual((expired["lifecycle"], expired["outcome_knowledge"]), ("Failed", "Known"), expired)
        self.assertEqual(expired["conditions"][0]["type"], "Fenced")
        self.still_original()

        self.configure(45)
        longer = n.submit("backup.create", target(self.volume))
        self.observe_partial()
        permit = self.permit(longer)
        span = datetime.fromisoformat(permit["not_after"]) - datetime.fromisoformat(permit["issued_at"])
        self.assertGreater(span.total_seconds(), 44)
        self.assertLessEqual(span.total_seconds(), 45)
        completed = n.wait(longer)
        self.assertEqual((completed["lifecycle"], completed["outcome_knowledge"]), ("Succeeded", "Known"), completed)

        # Keep this deadline below the deployment cap while allowing two real copies under CPU limits.
        deadline = (datetime.now(timezone.utc) + timedelta(seconds=35)).isoformat().replace("+00:00", "Z")
        request = n.call("Task.Prepare", {
            "tool": "backup.create", "input": target(self.volume), "key": "requested-capture-deadline",
            "delegation_id": "owner-delegation", "deadline_at": deadline,
        })
        bounded = n.call("Task.Submit", request)["task_id"]
        self.observe_partial()
        self.assertLessEqual(datetime.fromisoformat(self.permit(bounded)["not_after"]), datetime.fromisoformat(deadline))
        completed = n.wait(bounded)
        self.assertEqual((completed["lifecycle"], completed["outcome_knowledge"]), ("Succeeded", "Known"), completed)

    def test_revocation_interrupts_capture_and_invalid_window_is_rejected(self):
        n = self.n
        grant = n.call("Grant.Issue", {"principal_id": "local-owner", "units": 1, "ttl_seconds": 180,
            "actions": ["backup.create", "task.submit", "task.read"], "tool_targets": [self.volume["resource_id"]]})
        prepared = n.prepare("backup.create", target(self.volume), delegation=grant["delegation_ref"]["resource_id"])
        task = n.call("Task.Submit", prepared)["task_id"]
        self.observe_partial()
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        revoked = n.wait(task)
        self.assertEqual((revoked["lifecycle"], revoked["outcome_knowledge"]), ("Failed", "Known"), revoked)
        self.assertIn(revoked["conditions"][0]["type"], ("Fenced", "PermissionDenied"))
        self.still_original()

        for seconds in (0, 86_401):
            self.configure(seconds, start=False)
            invalid = subprocess.run(n.daemon + ["check"], capture_output=True, text=True, timeout=10)
            self.assertNotEqual(invalid.returncode, 0)
            self.assertIn("InvalidArgument", invalid.stderr)


if __name__ == "__main__":
    unittest.main()
