"""Exercise event-offset custody and cleanup over the daemon's Unix socket."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
import signal
import socket
import sqlite3
import struct
from threading import Barrier
import time
import unittest

from support import Failure, Node, key, target


class EventOffsetCapacityTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as failure:
            self.node.call(method, params, token)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def cursor(self, token=None, **scope):
        params = scope or {"kind": "Profile", "id": "profile-local"}
        return self.node.call("Resource.List", params, token)["watch_start_cursor"]

    def ack(self, consumer, cursor, previous=None, token=None):
        return self.node.call("Event.Ack", {"consumer": consumer, "previous": previous, "cursor": cursor}, token)

    def offset(self, consumer, token=None):
        return self.node.call("Event.Offset", {"id": consumer, "include_revision": True}, token)

    def delete(self, consumer, revision):
        return self.node.call("Event.OffsetDelete", {"consumer": consumer, "expected_revision": revision})

    def race(self, *calls):
        start = Barrier(len(calls))

        def attempt(call, *args):
            start.wait(timeout=5)
            try:
                return call(*args)
            except Failure as failure:
                return failure.code

        with ThreadPoolExecutor(max_workers=len(calls)) as pool:
            pending = [pool.submit(attempt, *call) for call in calls]
            return [result.result(timeout=10) for result in pending]

    def send_without_response(self, method, params):
        request = json.dumps({"schema_version": "1.0.0", "token": self.node.owner,
                              "request_id": key(), "method": method, "params": params}).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(5)
            stream.connect(str(self.node.socket))
            stream.sendall(struct.pack(">I", len(request)) + request)
            stream.shutdown(socket.SHUT_WR)

    def test_lost_delete_response_and_recreation_with_identical_cursor(self):
        n = self.node
        consumer, cursor = "lost-response", self.cursor()
        self.assertEqual(self.ack(consumer, cursor), {"cursor": cursor})
        first = self.offset(consumer)
        self.assertEqual(first["cursor"], cursor)
        self.assertTrue(first["revision"])
        self.assertEqual(n.call("Event.Offset", {"id": consumer}), {"cursor": cursor})
        self.assertEqual(self.ack(consumer, cursor), {"cursor": cursor})
        self.assertEqual(self.offset(consumer), first)

        request = {"consumer": consumer, "expected_revision": first["revision"]}
        self.send_without_response("Event.OffsetDelete", request)
        deadline = time.monotonic() + 5
        while True:
            observed = self.offset(consumer)
            if observed["cursor"] is None:
                break
            self.assertLess(time.monotonic(), deadline, observed)
            time.sleep(0.02)
        self.assertEqual(observed, {"cursor": None, "revision": None})
        self.assertEqual(n.call("Event.OffsetDelete", request), {"deleted": False})
        removed = [entry for entry in n.call("Audit.List", {"limit": 128})
                   if entry["method"] == "Event.OffsetDelete" and entry["decision"] == "Deleted"]
        self.assertEqual([json.loads(entry["resource"]) for entry in removed], [["local-owner", consumer]])

        n.stop(signal.SIGKILL)
        n.start()
        self.ack(consumer, cursor)
        recreated = self.offset(consumer)
        self.assertEqual(recreated["cursor"], first["cursor"])
        self.assertNotEqual(recreated["revision"], first["revision"])
        self.rejected("Conflict", "Event.OffsetDelete", request)
        self.assertEqual(self.offset(consumer), recreated)
        owner_token = n.witness / "owner.token"
        self.assertEqual(n.cli(owner_token, "event-offset", consumer), recreated)
        self.assertEqual(n.cli(owner_token, "event-offsets", "--limit", "1"), {
            "items": [{"consumer": consumer, "revision": recreated["revision"]}], "next_consumer": None,
        })
        self.assertEqual(n.cli(owner_token, "event-offset-delete", consumer, "--expected-revision",
                               recreated["revision"]), {"deleted": True})
        self.assertEqual(self.offset(consumer), {"cursor": None, "revision": None})

    def test_delete_and_advance_use_the_same_conditional_position(self):
        consumer, cursor = "competing-offset", self.cursor()
        self.ack(consumer, cursor)
        prior = self.offset(consumer)
        next_cursor = self.cursor()
        self.assertNotEqual(next_cursor, cursor)
        deletion, advancement = self.race(
            (self.delete, consumer, prior["revision"]), (self.ack, consumer, next_cursor, cursor),
        )
        latest = self.offset(consumer)
        if deletion == {"deleted": True}:
            self.assertEqual(advancement, "Conflict")
            self.assertEqual(latest, {"cursor": None, "revision": None})
        else:
            self.assertEqual(deletion, "Conflict")
            self.assertEqual(advancement, {"cursor": next_cursor})
            self.assertEqual(latest["cursor"], next_cursor)
            self.assertNotEqual(latest["revision"], prior["revision"])

    def test_per_principal_quota_keeps_existing_offsets_advanceable(self):
        n = self.node
        cursor = self.cursor()
        names = [f"quota-{number:03d}" for number in range(127)]
        for name in names:
            self.ack(name, cursor)
        candidates = ("quota-127", "quota-128")
        outcomes = self.race(*[(self.ack, candidate, cursor) for candidate in candidates])
        self.assertCountEqual(outcomes, [{"cursor": cursor}, "QuotaExceeded"])
        names.append(candidates[outcomes.index({"cursor": cursor})])
        rejected = candidates[outcomes.index("QuotaExceeded")]
        self.rejected("QuotaExceeded", "Event.Ack", {"consumer": rejected, "previous": None, "cursor": cursor})

        first = self.offset(names[0])
        self.ack(names[0], cursor)
        self.assertEqual(self.offset(names[0]), first)
        next_cursor = self.cursor()
        self.ack(names[0], next_cursor, cursor)
        latest = self.offset(names[0])
        self.assertEqual(latest["cursor"], next_cursor)
        self.assertNotEqual(latest["revision"], first["revision"])
        self.rejected("Conflict", "Event.OffsetDelete", {"consumer": names[0], "expected_revision": first["revision"]})

        after, found = None, []
        while True:
            params = {"limit": 19}
            if after is not None:
                params["after"] = after
            page = n.call("Event.OffsetList", params)
            self.assertLessEqual(len(page["items"]), 19)
            for item in page["items"]:
                self.assertEqual(set(item), {"consumer", "revision"})
            found.extend(page["items"])
            if page["next_consumer"] is None:
                break
            self.assertEqual(page["next_consumer"], page["items"][-1]["consumer"])
            if after is not None:
                self.assertGreater(page["next_consumer"], after)
            after = page["next_consumer"]
        self.assertEqual([item["consumer"] for item in found], names)
        self.assertEqual(found[0]["revision"], latest["revision"])

        principal, _ = n.agent(["event.read", "resource.read", "task.read"])
        token = principal["token"]
        agent_cursor = self.cursor(token)
        self.assertEqual(self.ack(names[0], agent_cursor, token=token), {"cursor": agent_cursor})
        self.assertEqual(n.call("Event.OffsetList", {}, token)["items"][0]["consumer"], names[0])
        self.assertEqual(self.offset(names[0]), latest)

        self.assertEqual(self.delete(names[0], latest["revision"]), {"deleted": True})
        owner_cursor = self.cursor()
        self.assertEqual(self.ack(rejected, owner_cursor), {"cursor": owner_cursor})
        self.assertEqual(self.offset(rejected)["cursor"], owner_cursor)

    def test_administrator_can_remove_revoked_owner_offset_after_scope_expiry(self):
        n = self.node
        agent, _ = n.agent(["event.read", "resource.read", "task.read"])
        other, _ = n.agent(["event.read", "resource.read", "task.read"])
        owner, token = agent["principal_id"], agent["token"]
        consumer = "expired-long-scope"
        cursor = self.cursor(token, kind="HostService", id="a" * 200)
        self.assertTrue(cursor.startswith("scope1."))
        self.ack(consumer, cursor, token=token)
        saved = self.offset(consumer, token)
        deletion = {"owner": owner, "consumer": consumer, "expected_revision": saved["revision"]}
        self.assertEqual(n.call("Event.OffsetList", {}, token), {
            "items": [{"consumer": consumer, "revision": saved["revision"]}], "next_consumer": None,
        })
        self.assertEqual(self.offset(consumer), {"cursor": None, "revision": None})
        self.rejected("PermissionDenied", "Event.OffsetList", {"owner": owner})
        self.rejected("PermissionDenied", "Event.OffsetDelete", deletion)
        self.rejected("PermissionDenied", "Event.OffsetList", {"owner": owner}, other["token"])
        self.rejected("PermissionDenied", "Event.OffsetDelete", deletion, other["token"])

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            expired = db.execute("UPDATE watch_cursor_scopes SET retain_until=0 WHERE owner=?", (owner,))
            self.assertEqual(expired.rowcount, 1)
        n.start()
        self.rejected("CursorExpired", "Resource.Watch", {"cursor": cursor}, token)
        self.assertEqual(self.offset(consumer, token), saved)
        n.call("Principal.Revoke", target(n.get(owner)["reference"]))
        self.assertEqual(n.call("Event.OffsetList", {"owner": owner}), {
            "items": [{"consumer": consumer, "revision": saved["revision"]}], "next_consumer": None,
        })
        self.assertEqual(n.call("Event.OffsetDelete", deletion), {"deleted": True})
        self.assertEqual(n.call("Event.OffsetDelete", deletion), {"deleted": False})
        self.assertEqual(n.call("Event.OffsetList", {"owner": owner}), {"items": [], "next_consumer": None})
