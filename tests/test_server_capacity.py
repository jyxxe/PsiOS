"""Exercise the daemon's real Linux descriptor ceiling without altering the host."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import resource
import socket
import time
import unittest

from support import Node


class ServerCapacityTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    @contextmanager
    def free_descriptors(self, available):
        pid = self.node.process.pid
        deadline = time.monotonic() + 2
        while True:
            entries = {}
            for path in Path(f"/proc/{pid}/fd").iterdir():
                try:
                    entries[int(path.name)] = os.readlink(path)
                except FileNotFoundError:
                    continue
            if sum(target.startswith("socket:") for target in entries.values()) == 1:
                break
            self.assertLess(time.monotonic(), deadline, entries)
            time.sleep(0.01)
        related = entries.keys()
        first_free = next(index for index in range(len(related) + 1) if index not in related)
        original = resource.prlimit(pid, resource.RLIMIT_NOFILE)
        resource.prlimit(pid, resource.RLIMIT_NOFILE, (first_free + available, original[1]))
        try:
            yield
        finally:
            if self.node.process.poll() is None:
                resource.prlimit(pid, resource.RLIMIT_NOFILE, original)

    def assert_ready(self, identity, pid):
        self.assertIsNone(self.node.process.poll())
        self.assertEqual(self.node.process.pid, pid)
        self.assertEqual(self.node.call("Principal.WhoAmI", {}), identity)

    def test_connection_dropped_when_socket_duplication_has_no_descriptor(self):
        node = self.node
        identity = node.call("Principal.WhoAmI", {})
        pid = node.process.pid
        with self.free_descriptors(1), socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(3)
            stream.connect(str(node.socket))
            self.assertEqual(stream.recv(1), b"")
        self.assert_ready(identity, pid)

    def test_accept_retries_after_descriptor_exhaustion(self):
        node = self.node
        identity = node.call("Principal.WhoAmI", {})
        pid = node.process.pid
        with self.free_descriptors(0), socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(0.25)
            stream.connect(str(node.socket))
            with self.assertRaises(TimeoutError):
                stream.recv(1)
            self.assertIsNone(node.process.poll())
        self.assert_ready(identity, pid)

    def test_shutdown_wakes_idle_workers_and_incomplete_connections(self):
        node = self.node
        identity = node.call("Principal.WhoAmI", {})
        node.stop()
        node.config.write_text(json.dumps(dict(json.loads(node.config.read_text()), tick_ms=5000)))
        node.start()
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(node.socket))
            client.sendall(b"\x00\x00")
            time.sleep(0.15)
            started = time.monotonic()
            node.stop()
            self.assertLess(time.monotonic() - started, 2.5)
            try:
                self.assertEqual(client.recv(1), b"")
            except ConnectionResetError:
                pass
        node.start()
        self.assertEqual(node.call("Principal.WhoAmI", {}), identity)
