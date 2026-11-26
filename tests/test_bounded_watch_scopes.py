"""Exercise bounded exact-watch scopes through the daemon's Unix socket."""
import base64
from contextlib import closing
import json
import os
import signal
import sqlite3
import unittest

from support import Failure, Node, target


class BoundedWatchScopes(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as failure:
            self.node.call(method, params, token)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def scope_id(self, cursor):
        self.assertLessEqual(len(cursor), 256)
        prefix, payload, _ = cursor.split(".")
        self.assertEqual(prefix, "scope1")
        data = base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4))
        return json.loads(data)[0]

    def configure_services(self, *units):
        n = self.node
        n.stop()
        config = json.loads(n.config.read_text())
        config["services"] = [{"unit": unit, "manager": "user"} for unit in units]
        n.config.write_text(json.dumps(config))
        n.start()

    def test_maximum_exact_filter_reuses_only_its_own_scope(self):
        n = self.node
        scope = {"kind": "HostService", "id": "a" * 200}
        first = n.call("Resource.List", scope)
        self.assertEqual(first["items"], [])
        self.assertTrue(first["complete"])
        self.assertIsNone(first["next_cursor"])
        cursor = first["watch_start_cursor"]
        lease = self.scope_id(cursor)

        again = n.call("Resource.List", scope)
        self.assertNotEqual(again["snapshot_id"], first["snapshot_id"])
        self.assertEqual(self.scope_id(again["watch_start_cursor"]), lease)
        other_id = n.call("Resource.List", dict(scope, id="a" * 199 + "b"))
        other_kind = n.call("Resource.List", dict(scope, kind="HostServiceState"))
        self.assertNotEqual(self.scope_id(other_id["watch_start_cursor"]), lease)
        self.assertNotEqual(self.scope_id(other_kind["watch_start_cursor"]), lease)
        self.rejected("InvalidArgument", "Resource.List", dict(scope, id="a" * 201))

        short = n.call("Resource.List", {"kind": "Profile", "id": "profile-local"})["watch_start_cursor"]
        self.assertFalse(short.startswith("scope1."))
        self.assertEqual(n.call("Resource.Watch", {"cursor": short})["events"], [])
        self.rejected("InvalidArgument", "Resource.Watch", {"cursor": cursor, "id": "profile-local"})
        self.rejected("CursorExpired", "Resource.List", {"cursor": cursor})
        polled = n.call("Resource.Watch", {"cursor": cursor})
        self.assertEqual(polled["events"], [])
        self.assertEqual(self.scope_id(polled["cursor"]), lease)

        principal, _ = n.agent(["event.read", "resource.read", "task.read"])
        token = principal["token"]
        their_cursor = n.call("Resource.List", scope, token)["watch_start_cursor"]
        self.assertNotEqual(self.scope_id(their_cursor), lease)
        self.assertEqual(n.call("Resource.Watch", {"cursor": their_cursor}, token)["events"], [])
        self.rejected("CursorExpired", "Resource.Watch", {"cursor": cursor}, token)
        self.rejected("CursorExpired", "Resource.Watch", {"cursor": their_cursor})

    def test_exact_service_updates_and_acknowledged_scope_survive_lease_cleanup(self):
        n = self.node
        unit, sibling = (letter * 112 + ".service" for letter in ("a", "b"))
        self.configure_services(unit, sibling)
        resource, other = "service-" + unit, "service-" + sibling
        scope = {"kind": "HostService", "id": resource}
        page = n.call("Resource.List", scope)
        original = n.get(resource)["reference"]
        self.assertEqual(page["items"], [original])
        initial_cursor = page["watch_start_cursor"]
        initial_lease = self.scope_id(initial_cursor)

        self.configure_services()
        retired = n.get(resource)
        self.assertEqual(retired["value"]["lifecycle"], "Unregistered")
        self.assertEqual(n.get(other)["value"]["lifecycle"], "Unregistered")
        retirement = n.call("Resource.Watch", {"cursor": initial_cursor})
        self.assertEqual([event["resource_ref"] for event in retirement["events"]], [retired["reference"]])

        self.configure_services(unit, sibling)
        registered = n.get(resource)
        self.assertEqual(registered["value"]["lifecycle"], "Registered")
        self.assertEqual(n.get(other)["value"]["lifecycle"], "Registered")
        registration = n.call("Event.Poll", {"cursor": retirement["cursor"]})
        self.assertEqual([event["resource_ref"] for event in registration["events"]], [registered["reference"]])
        self.assertEqual(self.scope_id(registration["cursor"]), initial_lease)
        request = {"consumer": "long-service", "previous": None, "cursor": registration["cursor"]}
        saved = n.call("Event.Ack", request)
        self.assertEqual(n.call("Event.Ack", request), saved)
        self.rejected("Conflict", "Event.Ack", dict(request, previous=saved["cursor"], cursor=retirement["cursor"]))

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            expired = db.execute("UPDATE watch_cursor_scopes SET retain_until=0 WHERE id=?", (initial_lease,))
            self.assertEqual(expired.rowcount, 1)
        n.start()
        self.assertEqual(n.call("Event.Offset", {"id": request["consumer"]}), saved)
        offset = n.call("Event.Offset", {"id": request["consumer"], "include_revision": True})
        recovered = n.cli(n.witness / "owner.token", "event-ack", request["consumer"], saved["cursor"])
        self.assertEqual(recovered, saved)
        self.assertEqual(n.call("Event.Offset", {"id": request["consumer"], "include_revision": True}), offset)
        commits = [event for event in n.call("Audit.List", {"limit": 128})
                   if event["method"] == "Event.Ack" and event["decision"] == "Allowed"]
        self.assertEqual(len(commits), 1)
        self.rejected("CursorExpired", "Event.Ack", dict(request, consumer="another-consumer"))
        self.rejected("CursorExpired", "Resource.Watch", saved)

        renewed = n.call("Resource.List", scope)
        renewed_lease = self.scope_id(renewed["watch_start_cursor"])
        self.assertNotEqual(renewed_lease, initial_lease)
        _, grant = n.agent(["task.read"])
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.rejected("CursorExpired", "Event.Ack", request)
        self.rejected("CursorExpired", "Resource.Watch", {"cursor": renewed["watch_start_cursor"]})
        self.assertEqual(n.call("Event.Offset", {"id": request["consumer"]}), saved)

        wrong = n.call("Resource.List", dict(scope, id=other))["watch_start_cursor"]
        self.rejected("Conflict", "Event.Ack", dict(request, previous=saved["cursor"], cursor=wrong))
        fresh = n.call("Resource.List", scope)
        self.assertNotEqual(fresh["authorization_epoch"], renewed["authorization_epoch"])
        self.assertEqual(fresh["items"], [registered["reference"]])
        self.assertNotIn(self.scope_id(fresh["watch_start_cursor"]), (initial_lease, renewed_lease))
        request = dict(request, previous=saved["cursor"], cursor=fresh["watch_start_cursor"])
        self.rejected("Conflict", "Event.Ack", dict(request, previous=None))
        updated = n.call("Event.Ack", request)
        self.assertEqual(n.call("Event.Offset", {"id": request["consumer"]}), updated)
        self.assertEqual(n.call("Resource.Watch", updated)["events"], [])
        self.configure_services()
        resumed = n.call("Resource.Watch", updated)
        self.assertEqual([event["resource_ref"] for event in resumed["events"]], [n.get(resource)["reference"]])
