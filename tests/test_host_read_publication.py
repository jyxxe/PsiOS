"""Pause a real host read after dispatch and before its result can be published."""

from contextlib import contextmanager
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest

from support import Node, target


@unittest.skipUnless(os.getenv("PSIOS_TEST_HOST_READ_BOUNDARY"), "requires strace and a private mount namespace")
class HostReadPublicationTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(os.geteuid(), 0)
        self.assertNotEqual(os.stat("/proc/self/ns/mnt").st_ino, os.stat("/proc/1/ns/mnt").st_ino)
        subprocess.run(["mount", "--make-rprivate", "/"], check=True, timeout=10)
        self.n = Node()
        self.addCleanup(self.n.close)
        self.node = self.n.call("Node.Get", {})["reference"]
        self.actor = self.n.call("Agent.Register", {"name": "inventory-observer"})

    def reading_meminfo(self):
        for descriptor in Path(f"/proc/{self.n.process.pid}/fd").iterdir():
            try:
                if os.readlink(descriptor) == "/proc/meminfo":
                    return True
            except FileNotFoundError:
                continue
        return False

    @contextmanager
    def sample(self):
        self.n.stop()
        daemon = self.n.daemon
        trace = self.n.base / "inventory-read.strace"
        # Delay the first genuine kernel read on this worker. A live descriptor
        # acknowledges that the Task reached the read before authority changes.
        self.n.daemon = ["strace", "-D", "-f", "-y", "-o", str(trace), "-P", "/proc/meminfo",
                         "-e", "trace=read", "-e", "inject=read:delay_exit=4s:when=1", "--", *daemon]
        try:
            self.n.start()
        finally:
            self.n.daemon = daemon
        grant = self.n.call("Grant.Issue", {"principal_id": self.actor["principal_id"], "units": 2,
            "actions": ["task.submit", "task.read", "node.inventory"],
            "tool_targets": [self.node["resource_id"]], "ttl_seconds": 120})
        token = self.actor["token"]
        prepared = self.n.prepare("node.inventory", {}, token=token,
                                  delegation=grant["delegation_ref"]["resource_id"])
        task = self.n.call("Task.Submit", prepared, token)["task_id"]
        deadline = time.monotonic() + 8
        while not self.reading_meminfo():
            self.assertLess(time.monotonic(), deadline, "the inventory worker did not open genuine meminfo")
            time.sleep(0.02)
        self.assertEqual(self.n.call("Task.Get", {"id": task})["lifecycle"], "Running")
        yield task, grant

    def test_current_permit_publishes_the_exact_observation(self):
        with self.sample() as (task, _):
            pass
        completed = self.n.wait(task)
        self.assertEqual((completed["lifecycle"], completed["outcome_knowledge"]), ("Succeeded", "Known"), completed)
        reference, = completed["result"]["artifact_refs"]
        value = self.n.call("Artifact.Read", {"reference": reference})
        fields = dict(line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())
        for name in ("MemTotal", "SwapTotal"):
            self.assertEqual(value["memory_bytes"][name], int(fields[name].split()[0]) * 1024)
        self.assertEqual(value["kernel_observation"]["memory"], "Observed")
        self.assertIn("(DELAYED)", (self.n.base / "inventory-read.strace").read_text())

    def assert_unpublished(self, task, lifecycle, code):
        completed = self.n.wait(task)
        self.assertEqual((completed["lifecycle"], completed["outcome_knowledge"]), (lifecycle, "Known"), completed)
        self.assertEqual(completed["result"]["artifact_refs"], [])
        self.assertEqual([condition["type"] for condition in completed["conditions"]], [code], completed)
        self.assertEqual(self.n.call("Resource.List", {"kind": "Artifact"})["items"], [])
        return completed

    def test_revocation_before_publication_remains_fenced_after_restart(self):
        with self.sample() as (task, grant):
            self.n.call("Grant.Revoke", target(grant["grant_ref"]))
        closed = self.assert_unpublished(task, "Failed", "PermissionDenied")
        self.n.stop(signal.SIGKILL)
        self.n.start()
        self.assertEqual(self.n.call("Task.Get", {"id": task}), closed)

    def test_cancel_before_publication_discards_the_observation(self):
        with self.sample() as (task, _):
            self.n.call("Task.Cancel", target(self.n.get(task)["reference"]), self.actor["token"])
        self.assert_unpublished(task, "Cancelled", "Fenced")

    def test_revoked_read_error_uses_only_the_authority_denial(self):
        replacement = self.n.base / "foreign-version"
        replacement.write_bytes(Path("/proc/version").read_bytes())
        subprocess.run(["mount", "--bind", replacement, "/proc/version"], check=True, timeout=10)
        try:
            with self.sample() as (task, grant):
                self.n.call("Grant.Revoke", target(grant["grant_ref"]))
            self.assert_unpublished(task, "Failed", "PermissionDenied")
        finally:
            subprocess.run(["umount", "/proc/version"], check=True, timeout=10)

    def test_target_revision_change_prevents_publication(self):
        with self.sample() as (task, _):
            self.n.call("Node.Configure", {"expected_resource_version": self.node["version"], "scheduling": "Cordoned"})
        self.assert_unpublished(task, "Failed", "Fenced")
