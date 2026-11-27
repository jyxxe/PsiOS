"""Pause a real host read after dispatch and before its result can be published."""

from contextlib import contextmanager
import errno
import os
import signal
import subprocess
import time
import unittest

from support import Node, target


@unittest.skipUnless(os.getenv("PSIOS_TEST_HOST_READ_BOUNDARY"), "requires a private Linux mount namespace")
class HostReadPublicationTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(os.geteuid(), 0)
        self.assertNotEqual(os.stat("/proc/self/ns/mnt").st_ino, os.stat("/proc/1/ns/mnt").st_ino)
        subprocess.run(["mount", "--make-rprivate", "/"], check=True, timeout=10)
        self.n = Node()
        self.addCleanup(self.n.close)
        self.fifo = self.n.base / "memory-info"
        os.mkfifo(self.fifo, mode=0o600)
        subprocess.run(["mount", "--bind", self.fifo, "/proc/meminfo"], check=True, timeout=10)
        self.addCleanup(subprocess.run, ["umount", "/proc/meminfo"], check=True, timeout=10)
        self.node = self.n.call("Node.Get", {})["reference"]
        self.actor = self.n.call("Agent.Register", {"name": "inventory-observer"})

    @contextmanager
    def sample(self, payload=b"MemTotal: 123456 kB\nSwapTotal: 0 kB\n"):
        grant = self.n.call("Grant.Issue", {"principal_id": self.actor["principal_id"], "units": 2,
            "actions": ["task.submit", "task.read", "node.inventory"],
            "tool_targets": [self.node["resource_id"]], "ttl_seconds": 120})
        token = self.actor["token"]
        prepared = self.n.prepare("node.inventory", {}, token=token,
                                  delegation=grant["delegation_ref"]["resource_id"])
        task = self.n.call("Task.Submit", prepared, token)["task_id"]
        # O_NONBLOCK acknowledges the daemon's read end without timing its start.
        deadline = time.monotonic() + 8
        while True:
            try:
                writer = os.open(self.fifo, os.O_WRONLY | os.O_NONBLOCK)
                break
            except OSError as error:
                if error.errno != errno.ENXIO or time.monotonic() >= deadline:
                    raise
                time.sleep(0.02)
        try:
            self.assertEqual(self.n.call("Task.Get", {"id": task})["lifecycle"], "Running")
            yield task, grant
            os.write(writer, payload)
        finally:
            os.close(writer)

    def test_current_permit_publishes_the_exact_observation(self):
        with self.sample() as (task, _):
            pass
        completed = self.n.wait(task)
        self.assertEqual((completed["lifecycle"], completed["outcome_knowledge"]), ("Succeeded", "Known"), completed)
        reference, = completed["result"]["artifact_refs"]
        value = self.n.call("Artifact.Read", {"reference": reference})
        self.assertEqual(value["memory_bytes"], {"MemTotal": 123456 * 1024, "SwapTotal": 0})

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
        with self.sample(payload=b"\xff") as (task, grant):
            self.n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assert_unpublished(task, "Failed", "PermissionDenied")

    def test_target_revision_change_prevents_publication(self):
        with self.sample() as (task, _):
            self.n.call("Node.Configure", {"expected_resource_version": self.node["version"], "scheduling": "Cordoned"})
        self.assert_unpublished(task, "Failed", "Fenced")
