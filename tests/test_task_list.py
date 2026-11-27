"""Read an authorized history page while the live scheduler keeps changing."""
import signal
import unittest

from support import Failure, Node, target


class TaskListing(unittest.TestCase):
    def setUp(self):
        self.node = Node()
        self.addCleanup(self.node.close)

    def refused(self, code, fields, token=None):
        with self.assertRaises(Failure) as error:
            self.node.call("Task.List", fields, token)
        self.assertEqual(error.exception.code, code)

    def test_snapshot_keeps_original_lifecycle_and_exact_revision_after_restart(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "task-list-data"})

        def waiting(name):
            task = n.submit("volume.write", {**target(volume), "name": name, "content": "fixture"})
            self.assertEqual(n.wait(task)["lifecycle"], "WaitingApproval")
            return n.get(task)["reference"]

        first, second = waiting("first"), waiting("second")
        query = dict(task_type="volume.write", lifecycle="WaitingApproval", limit=1)
        page = n.cli(n.witness / "owner.token", "task-list", "--tool", "volume.write",
                     "--lifecycle", "WaitingApproval", "--limit", "1")
        self.assertFalse(page["complete"])
        self.assertEqual([item["task_ref"] for item in page["items"]], [first])
        self.assertEqual(page["items"][0]["outcome_knowledge"], "NotStarted")
        self.assertEqual(page["items"][0]["result_availability"], "Pending")
        cursor = page["next_cursor"]
        self.refused("CursorExpired", dict(query, cursor=cursor, lifecycle="Cancelled"))
        self.refused("CursorExpired", dict(query, cursor=cursor, task_type="volume.create"))
        self.refused("CursorExpired", dict(query, cursor=cursor, owner_principal_id="local-owner"))
        self.refused("InvalidArgument", dict(query, limit=65))
        self.refused("InvalidArgument", dict(query, lifecycle="bogus"))

        n.call("Task.Cancel", target(second))
        self.assertEqual(n.wait(second["resource_id"])["lifecycle"], "Cancelled")
        third = waiting("later")
        n.stop(signal.SIGKILL)
        n.start()
        continued = n.call("Task.List", dict(query, cursor=cursor))
        self.assertTrue(continued["complete"])
        self.assertIsNone(continued["next_cursor"])
        self.assertEqual([item["task_ref"] for item in continued["items"]], [second])
        self.assertEqual(continued["items"][0]["lifecycle"], "WaitingApproval")
        exact = n.call("Resource.Resolve", {"reference": second})
        self.assertEqual(exact["value"]["lifecycle"], "WaitingApproval")
        current = n.call("Task.List", dict(query, limit=64))
        self.assertEqual([item["task_ref"] for item in current["items"]], [first, third])

    def test_agent_lists_own_tasks_and_loses_access_to_an_existing_cursor(self):
        n = self.node
        agent, grant = n.agent(["task.submit", "task.read", "system.status"])
        owner_task = n.submit("system.status", {})
        self.assertEqual(n.wait(owner_task)["lifecycle"], "Succeeded")
        delegation = grant["delegation_ref"]["resource_id"]
        ids = []
        for _ in range(2):
            request = n.prepare("system.status", {}, token=agent["token"], delegation=delegation)
            ids.append(n.call("Task.Submit", request, agent["token"])["task_id"])
            self.assertEqual(n.wait(ids[-1])["lifecycle"], "Succeeded")
        n.call("Task.Archive", target(n.get(ids[0])["reference"]))
        own = n.call("Task.List", {"limit": 1}, agent["token"])
        cursor = own["next_cursor"]
        self.assertEqual([item["task_ref"]["resource_id"] for item in own["items"]], ids[:1])
        follow = n.call("Task.List", {"cursor": cursor}, agent["token"])
        self.assertEqual([item["task_ref"]["resource_id"] for item in follow["items"]], ids[1:])
        named = {"owner_principal_id": agent["principal_id"], "lifecycle": "Succeeded"}
        operator = n.call("Task.List", named)
        self.assertEqual([item["task_ref"]["resource_id"] for item in operator["items"]], ids)
        self.assertTrue(all(item["owner_principal_id"] == agent["principal_id"] for item in operator["items"]))
        self.refused("PermissionDenied", {"owner_principal_id": "local-owner"}, agent["token"])
        self.refused("CursorExpired", {"owner_principal_id": agent["principal_id"], "cursor": cursor})
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.refused("PermissionDenied", {"cursor": cursor}, agent["token"])

    def test_empty_filtered_page_advances_over_the_bounded_candidate_window(self):
        n = self.node
        for _ in range(128):
            task = n.submit("system.status", {})
            self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
        task = n.submit("volume.create", {"name": "after-scan-window"})
        self.assertEqual(n.wait(task, autoapprove=True)["lifecycle"], "Succeeded")
        query = dict(task_type="volume.create", lifecycle="Succeeded")
        first = n.call("Task.List", query)
        self.assertFalse(first["complete"])
        self.assertEqual(first["items"], [])
        second = n.call("Task.List", dict(query, cursor=first["next_cursor"]))
        self.assertTrue(second["complete"])
        self.assertEqual([item["task_ref"]["resource_id"] for item in second["items"]], [task])
