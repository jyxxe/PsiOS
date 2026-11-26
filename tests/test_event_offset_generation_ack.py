"""Exercise generation-checked watch acknowledgments with real socket clients."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import socket
import struct
import subprocess
from threading import Barrier
import time
import unittest

from support import Failure, Node, key


class EventOffsetGenerationAck(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def cursor(self, scope=None):
        if scope is None:
            scope = {"kind": "Profile", "id": "profile-local"}
        return self.node.call("Resource.List", scope)["watch_start_cursor"]

    def offset(self, consumer):
        return self.node.call("Event.Offset", {"id": consumer, "include_revision": True})

    def checked(self, consumer, prior, cursor):
        return {"consumer": consumer, "previous": prior["cursor"], "cursor": cursor,
                "expected_revision": prior["revision"]}

    def create(self, consumer, cursor):
        self.assertEqual(self.node.call("Event.Ack", {"consumer": consumer, "cursor": cursor}),
                         {"cursor": cursor})
        current = self.offset(consumer)
        self.assertEqual(current["cursor"], cursor)
        self.assertIsInstance(current["revision"], str)
        return current

    def delete(self, consumer, current):
        self.assertEqual(self.node.call("Event.OffsetDelete", {"consumer": consumer,
                         "expected_revision": current["revision"]}), {"deleted": True})

    def reject(self, code, request):
        with self.assertRaises(Failure) as failure:
            self.node.call("Event.Ack", request)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def audit_count(self):
        return sum(row["method"] == "Event.Ack" and row["decision"] == "Allowed"
                   for row in self.node.call("Audit.List", {"limit": 128}))

    def send_without_response(self, request):
        message = json.dumps({"schema_version": "1.0.0", "request_id": key(), "token": self.node.owner,
                              "method": "Event.Ack", "params": request}).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(5)
            stream.connect(str(self.node.socket))
            stream.sendall(struct.pack(">I", len(message)) + message)
            stream.shutdown(socket.SHUT_WR)

    def test_two_clients_cannot_advance_across_a_deleted_and_recreated_generation(self):
        node, consumer = self.node, "recreated"
        token = node.witness / "owner.token"
        node_ref = node.call("Node.Get", {})["reference"]
        previous = self.cursor({"kind": "Node", "id": node_ref["resource_id"]})
        first = self.create(consumer, previous)
        changed = node.call("Node.Configure", {"expected_resource_version": node_ref["version"],
                                                "scheduling": "Cordoned"})
        page = node.call("Resource.Watch", {"cursor": previous})
        self.assertEqual([event["resource_ref"] for event in page["events"]], [changed])
        next_cursor = page["cursor"]
        captured = self.checked(consumer, first, next_cursor)
        self.assertEqual(node.cli(token, "event-offset", consumer), first)

        self.assertEqual(node.cli(token, "event-offset-delete", consumer,
                                 "--expected-revision", first["revision"]), {"deleted": True})
        self.reject("Conflict", captured)
        recreated = self.create(consumer, previous)
        self.assertNotEqual(recreated["revision"], first["revision"])
        self.reject("Conflict", captured)
        self.assertEqual(self.offset(consumer), recreated)

        other = self.create("other-consumer", previous)
        self.reject("Conflict", dict(captured, expected_revision=other["revision"]))
        self.reject("InvalidArgument", dict(captured, expected_revision=None))
        self.reject("InvalidArgument", dict(captured, previous=None))
        current = node.cli(token, "event-ack", consumer, next_cursor, "--previous", previous,
                           "--expected-revision", recreated["revision"])
        self.assertEqual(current, self.offset(consumer))
        self.assertEqual(current["cursor"], next_cursor)
        self.assertNotEqual(current["revision"], recreated["revision"])
        self.assertEqual(self.offset("other-consumer"), other)
        self.assertEqual(node.call("Resource.Watch", {"cursor": current["cursor"]})["events"], [])

        count = self.audit_count()
        self.assertEqual(node.call("Event.Ack", self.checked(consumer, current, next_cursor)), current)
        self.assertEqual(node.call("Event.Ack", {"consumer": consumer, "cursor": next_cursor}),
                         {"cursor": next_cursor})
        self.assertEqual(self.audit_count(), count)

    def test_lost_reply_replays_only_its_exact_current_successor_across_sigkill(self):
        node, consumer = self.node, "lost-checked"
        scope = {"kind": "HostService", "id": "s" * 200}
        first = self.create(consumer, self.cursor(scope))
        next_cursor = self.cursor(scope)
        self.assertTrue(next_cursor.startswith("scope1."))
        self.assertNotEqual(next_cursor, first["cursor"])
        request = self.checked(consumer, first, next_cursor)
        count = self.audit_count()
        self.send_without_response(request)
        deadline = time.monotonic() + 6
        while (current := self.offset(consumer))["cursor"] != next_cursor:
            self.assertLess(time.monotonic(), deadline, current)
            time.sleep(0.02)
        self.assertNotEqual(current["revision"], first["revision"])
        self.assertEqual(self.audit_count(), count + 1)

        node.stop(signal.SIGKILL)
        report = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertEqual(report["sqlite_integrity"], "Verified", report)
        node.start()
        self.assertEqual(node.call("Event.Ack", request), current)
        self.assertEqual(self.audit_count(), count + 1)
        alternate = self.cursor(scope)
        self.reject("Conflict", dict(request, previous=alternate))
        self.reject("Conflict", dict(request, cursor=alternate))
        self.assertEqual(self.offset(consumer), current)

        self.delete(consumer, current)
        recreated = self.create(consumer, next_cursor)
        self.assertNotEqual(recreated["revision"], current["revision"])
        self.reject("Conflict", request)
        self.assertEqual(self.offset(consumer), recreated)
        self.assertEqual(self.audit_count(), count + 2)

    def test_competing_checked_advances_still_require_current_watch_authority(self):
        node, consumer = self.node, "competing-checked"
        first = self.create(consumer, self.cursor())
        self.reject("Conflict", self.checked(consumer, first, self.cursor({"kind": "Node"})))
        proposals = [self.checked(consumer, first, self.cursor()) for _ in range(2)]
        gate = Barrier(2)

        def race(request):
            gate.wait(timeout=5)
            try:
                return node.call("Event.Ack", request)
            except Failure as error:
                return error.code

        with ThreadPoolExecutor(max_workers=2) as pool:
            outcomes = list(pool.map(race, proposals))
        winner = self.offset(consumer)
        self.assertCountEqual(outcomes, [winner, "Conflict"])
        committed = proposals[outcomes.index(winner)]
        self.assertEqual(node.call("Event.Ack", committed), winner)

        node.agent(["event.read", "resource.read", "task.read"])
        self.reject("CursorExpired", committed)
        self.assertEqual(self.offset(consumer), winner)
        next_cursor = self.cursor()
        advanced = node.call("Event.Ack", self.checked(consumer, winner, next_cursor))
        self.assertEqual(advanced, self.offset(consumer))
        self.assertEqual(advanced["cursor"], next_cursor)
