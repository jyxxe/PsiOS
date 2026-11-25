"""Exercise exact resource watches and their authorization on the local transport."""
from concurrent.futures import ThreadPoolExecutor, TimeoutError
import unittest

from support import Failure, Node, target


class ResourceWatchTests(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)
        reference = self.node.call("Node.Get", {})["reference"]
        self.node.call("Node.Configure", {"expected_resource_version": reference["version"], "scheduling": "Cordoned"})

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as failure:
            self.node.call(method, params, token)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def test_scoped_socket_and_cli_watch_only_the_selected_task(self):
        n = self.node
        first = n.submit("system.status", {})
        other = n.submit("system.status", {})
        scope = {"kind": "Task", "id": first}
        initial = n.call("Resource.List", scope)
        self.assertEqual(initial["items"], [n.get(first)["reference"]])
        self.assertTrue(initial["complete"])
        cli = n.cli(n.witness / "owner.token", "list", "Task", "--id", first)
        self.assertEqual(cli["items"], initial["items"])
        n.call("Task.Cancel", target(n.get(other)["reference"]))
        cursor = initial["watch_start_cursor"]
        self.assertEqual(n.call("Resource.Watch", {"cursor": cursor})["events"], [])
        with ThreadPoolExecutor(max_workers=1) as workers:
            waiting = workers.submit(n.call, "Resource.Watch", {"cursor": cursor, "timeout_ms": 2000})
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.08)
            n.call("Task.Cancel", target(n.get(first)["reference"]))
            polled = waiting.result(timeout=3)
        event, = polled["events"]
        self.assertEqual(event["resource_ref"], n.get(first)["reference"])
        self.assertEqual(n.cli(n.witness / "owner.token", "watch", cli["watch_start_cursor"], "--wait-ms", "2000")["events"], [event])
        self.assertEqual(n.call("Event.Poll", {"cursor": cursor})["events"], [event])

        checkpoint = {"consumer": "specific-task", "previous": None, "cursor": polled["cursor"]}
        saved = n.call("Event.Ack", checkpoint)
        other_cursor = n.call("Resource.List", dict(scope, id=other))["watch_start_cursor"]
        self.rejected("Conflict", "Event.Ack", dict(checkpoint, previous=saved["cursor"], cursor=other_cursor))
        n.stop()
        n.start()
        self.assertEqual(n.call("Event.Offset", {"id": "specific-task"}), saved)
        self.assertEqual(n.call("Resource.Watch", saved)["events"], [])

    def test_scope_cannot_be_broadened_and_revocation_interrupts_wait(self):
        n = self.node
        owner_task = n.submit("system.status", {})
        page = n.call("Resource.List", {"kind": "Task", "id": owner_task})
        self.rejected("InvalidArgument", "Resource.Watch", {"cursor": page["watch_start_cursor"], "id": "profile-local"})
        n.submit("system.status", {})
        continued = n.call("Resource.List", {"kind": "Task", "limit": 1})["next_cursor"]
        self.rejected("CursorExpired", "Resource.List", {"cursor": continued, "id": owner_task})

        agent, grant = n.agent(["event.read", "resource.read", "task.read", "task.submit", "system.status"])
        token = agent["token"]
        hidden = n.call("Resource.List", {"id": owner_task}, token)
        self.assertEqual(hidden["items"], [])
        self.rejected("CursorExpired", "Resource.Watch", {"cursor": page["watch_start_cursor"]}, token)
        cursor = n.call("Resource.List", {"kind": "Profile", "id": "profile-local"}, token)["watch_start_cursor"]
        with ThreadPoolExecutor(max_workers=1) as workers:
            waiting = workers.submit(n.call, "Resource.Watch", {"cursor": cursor, "timeout_ms": 3000}, token)
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.08)
            n.call("Grant.Revoke", target(grant["grant_ref"]))
            with self.assertRaises(Failure) as denied:
                waiting.result(timeout=3)
        self.assertEqual(denied.exception.code, "PermissionDenied")

    def test_principal_pages_keep_the_snapshot_while_new_events_are_filtered(self):
        n = self.node
        agent, grant = n.agent(["event.read", "resource.read", "task.read", "task.submit", "system.status"])
        token = agent["token"]
        delegation = grant["delegation_ref"]["resource_id"]

        def submit():
            request = n.prepare("system.status", {}, token=token, delegation=delegation)
            return n.call("Task.Submit", request, token)["task_id"]

        first = submit()
        private = n.submit("system.status", {})
        second = submit()
        old_second = n.get(second)["reference"]
        # Updating the earliest Task must not move it behind the later Task
        # when starting a new snapshot.
        n.call("Task.Cancel", target(n.get(first)["reference"]), token)
        before = n.call("Resource.List", {"kind": "Task", "limit": 1}, token)
        self.assertEqual(before["items"], [n.get(first)["reference"]])
        self.assertFalse(before["complete"])
        n.call("Task.Cancel", target(n.get(private)["reference"]))
        n.call("Task.Cancel", target(old_second), token)
        latest = n.get(second)["reference"]
        self.assertNotEqual(latest, old_second)

        final = n.call("Resource.List", {"cursor": before["next_cursor"], "limit": 1}, token)
        self.assertTrue(final["complete"])
        self.assertEqual(final["items"], [old_second])
        self.assertEqual(final["snapshot_id"], before["snapshot_id"])
        changes = n.call("Resource.Watch", {"cursor": final["watch_start_cursor"], "limit": 1}, token)
        self.assertEqual([event["resource_ref"] for event in changes["events"]], [latest])
        after = n.call("Event.Poll", {"cursor": changes["cursor"], "limit": 1}, token)
        self.assertEqual(after["events"], [])
