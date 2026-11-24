"""Verify blocking task reads, current grants and bounded wait admission."""
from concurrent.futures import ThreadPoolExecutor, TimeoutError
import json
import os
import socket
import struct
import time
import unittest

from support import Failure, Node, key, target


class TaskWaitTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.schedule("Cordoned")

    def schedule(self, mode):
        node = self.node.call("Node.Get", {})["reference"]
        self.node.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})

    def wait_task(self, task, timeout_ms, *, after=None, token=None):
        query = {"id": task, "timeout_ms": timeout_ms}
        if after is not None:
            query["after_version"] = after
        return self.node.call("Task.Wait", query, token)

    def wait_stream(self, task):
        node = self.node
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(5)
            stream.connect(str(node.socket))
            frame = json.dumps({"schema_version": "1.0.0", "request_id": key(), "token": node.owner,
                                "method": "Task.Wait", "params": {"id": task, "timeout_ms": 8000}}).encode()
            stream.sendall(struct.pack(">I", len(frame)) + frame)
            return stream.dup()

    def test_wait_completion_through_socket_and_cli(self):
        node = self.node
        task = node.submit("system.status", {})
        with ThreadPoolExecutor(max_workers=2) as threads:
            waiter = threads.submit(self.wait_task, task, 3000)
            cli = threads.submit(node.cli, node.witness / "owner.token", "wait", task, "--seconds", "3")
            with self.assertRaises(TimeoutError):
                waiter.result(timeout=0.08)
            self.schedule("Enabled")
            observed = waiter.result(timeout=3)
            self.assertEqual(observed, cli.result(timeout=3))
        self.assertEqual(observed["lifecycle"], "Succeeded")
        self.assertEqual(observed, node.call("Task.Get", {"id": task}))
        reference, = observed["result"]["artifact_refs"]
        self.assertTrue(node.call("Artifact.Read", {"reference": reference})["authority_id"])

    def test_wait_revisions_and_timeout_return_persisted_state(self):
        node = self.node
        task = node.submit("system.status", {})
        initial = node.call("Task.Get", {"id": task})
        revision = initial["task_revision"]
        began = time.monotonic()
        self.assertEqual(self.wait_task(task, 60, after=revision), initial)
        self.assertGreaterEqual(time.monotonic() - began, 0.05)
        for timeout in (0, 10_001, "60", None):
            with self.assertRaises(Failure) as rejected:
                self.wait_task(task, timeout)
            self.assertEqual(rejected.exception.code, "InvalidArgument")
        for after, code in [("r001", "InvalidArgument"), ("r9223372036854775807", "PreconditionFailed")]:
            with self.assertRaises(Failure) as rejected:
                self.wait_task(task, 60, after=after)
            self.assertEqual(rejected.exception.code, code)
        with ThreadPoolExecutor(max_workers=1) as threads:
            waiter = threads.submit(self.wait_task, task, 3000, after=revision)
            with self.assertRaises(TimeoutError):
                waiter.result(timeout=0.08)
            node.call("Task.Cancel", {"id": task, "expected_resource_version": revision})
            cancelled = waiter.result(timeout=3)
        self.assertEqual(cancelled["lifecycle"], "Cancelled")
        self.assertGreater(int(cancelled["task_revision"][1:]), int(revision[1:]))
        node.stop()
        node.start()
        self.assertEqual(self.wait_task(task, 1000), cancelled)
        self.assertEqual(self.wait_task(task, 1000, after=revision), cancelled)

    def test_revoke_read_access_from_inflight_waiter(self):
        node = self.node
        agent, grant = node.agent(["task.submit", "task.read", "system.status"])
        token = agent["token"]
        request = node.prepare("system.status", {}, token=token, delegation=grant["delegation_ref"]["resource_id"])
        task = node.call("Task.Submit", request, token)["task_id"]
        outside, _ = node.agent(["task.read"])
        with self.assertRaises(Failure) as denied:
            self.wait_task(task, 60, token=outside["token"])
        self.assertEqual(denied.exception.code, "PermissionDenied")

        with ThreadPoolExecutor(max_workers=1) as threads:
            waiter = threads.submit(self.wait_task, task, 3000, token=token)
            with self.assertRaises(TimeoutError):
                waiter.result(timeout=0.08)
            node.call("Grant.Revoke", target(grant["grant_ref"]))
            with self.assertRaises(Failure) as revoked:
                waiter.result(timeout=3)
            self.assertEqual(revoked.exception.code, "PermissionDenied")
        owner_view = node.call("Task.Get", {"id": task})
        self.assertEqual((owner_view["lifecycle"], owner_view["outcome_knowledge"]), ("Failed", "NotStarted"))

    def test_disconnect_releases_a_bounded_wait_slot(self):
        node = self.node
        task = node.submit("system.status", {})
        queued = node.call("Task.Get", {"id": task})
        streams = []
        try:
            for _ in range(8):
                streams.append(self.wait_stream(task))
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                try:
                    self.wait_task(task, 10)
                except Failure as full:
                    self.assertEqual(full.code, "QueueFull")
                    break
            else:
                self.fail("all wait slots should be occupied")
            with self.assertRaises(Failure) as event_wait:
                node.call("Event.Wait", {"limit": 1, "timeout_ms": 30})
            self.assertEqual(event_wait.exception.code, "QueueFull")

            # The SDK can still read an already-settled Task when all slots are held.
            other = node.submit("system.status", {})
            before = node.call("Task.Get", {"id": other})
            node.call("Task.Cancel", {"id": other, "expected_resource_version": before["task_revision"]})
            self.assertEqual(node.cli(node.witness / "owner.token", "wait", other, "--seconds", "2")["lifecycle"],
                             "Cancelled")
            streams.pop().close()
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                try:
                    self.assertEqual(self.wait_task(task, 20), queued)
                    break
                except Failure as full:
                    self.assertEqual(full.code, "QueueFull")
                    time.sleep(0.02)
            else:
                self.fail("disconnect should release the wait slot before the original timeout")
        finally:
            for stream in streams:
                stream.close()

    def test_write_half_close_still_delivers_the_receipt(self):
        node = self.node
        task = node.submit("system.status", {})
        with self.wait_stream(task) as stream:
            stream.shutdown(socket.SHUT_WR)
            waiting = node.call("Task.Get", {"id": task})
            node.call("Task.Cancel", {"id": task, "expected_resource_version": waiting["task_revision"]})
            with stream.makefile("rb") as response:
                length, = struct.unpack(">I", response.read(4))
                reply = json.loads(response.read(length))
        self.assertEqual(reply["result"], node.call("Task.Get", {"id": task}))
        self.assertEqual(reply["result"]["lifecycle"], "Cancelled")
