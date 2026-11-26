"""Discover exact reclaimable Task results over the private Unix API."""
from collections import Counter
from datetime import datetime, timedelta, timezone
import json
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key, target
from test_schedule import timestamp
from test_task_result_eviction_batch import APPLY, PREPARE, Observation


CANDIDATES = "Task.ResultEvictionCandidates"


class ResultEvictionCandidateTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.agent = self.node.call("Agent.Register", {"name": "candidate-result-contributor"})
        self.token = self.agent["token"]
        self.profile = self.node.get("profile-local")["reference"]
        self.execution = self.grant(self.agent, ["task.submit", "task.read", "system.status"],
                                    [self.profile["resource_id"]])["delegation_ref"]["resource_id"]

    def grant(self, actor, actions, targets):
        return self.node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions, "tool_targets": targets,
            "units": 12, "ttl_seconds": 300,
        })

    def denied(self, code, params, token=None):
        with self.assertRaises(Failure) as caught:
            self.node.call(CANDIDATES, params, token)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def observe(self, owner=False):
        n = self.node
        if owner:
            task_id = n.submit("system.status", {})
        else:
            request = n.prepare("system.status", {}, token=self.token, delegation=self.execution)
            task_id = n.call("Task.Submit", request, self.token)["task_id"]
        result = n.wait(task_id)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Succeeded", "Known"), result)
        artifact, = result["result"]["artifact_refs"]
        query = {"reference": artifact}
        payload = n.call("Artifact.Read", query)
        count = int(n.call("Artifact.Get", query)["resource"]["spec"]["size_bytes"])
        self.assertTrue(payload["authority_id"] and payload["kernel_release"], payload)
        return Observation(n.get(task_id)["reference"], result, artifact, payload, count)

    def create_project_pins(self):
        n = self.node
        project = n.call("Project.Create", {"key": key(), "title": "Keep in-review observations"})["reference"]
        project_id = project["resource_id"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [project_id])
        project = n.call("Project.SetMember", {
            **target(project), "principal_id": self.agent["principal_id"], "role": "Contributor",
        })["reference"]
        self.grant(self.agent, ["project.read", "project.contribute"], [project_id])
        items = []
        for title in ("Keep first observation", "Retain a newly submitted observation"):
            items.append(n.call("WorkItem.Create", {
                "project_id": project_id, "expected_project_version": project["version"],
                "key": key(), "title": title, "assignee_principal_id": self.agent["principal_id"],
                "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": self.profile},
            })["reference"])
        return items

    def submit_pin(self, item, observation):
        pending = self.node.call("WorkItem.SubmitEvidence", {
            **target(item), "task_ref": observation.task, "artifact_ref": observation.artifact,
        }, self.token)
        self.assertEqual(pending["value"]["lifecycle"], "InReview", pending)
        return pending["reference"]

    def page(self, query, **changes):
        page = self.node.call(CANDIDATES, {**query, **changes})
        self.assertEqual(set(page["skipped"]), {"changed", "ineligible", "pinned"}, page)
        self.assertEqual(page["scanned"], len(page["items"]) + sum(page["skipped"].values()), page)
        self.assertLessEqual(page["scanned"], 256, page)
        return page

    def remainder(self, query, cursor, limit):
        items = []
        skipped = Counter()
        seen = set()
        while cursor is not None:
            self.assertNotIn(cursor, seen)
            self.assertLess(len(seen), 16)
            seen.add(cursor)
            page = self.page(query, cursor=cursor, limit=limit)
            items.extend(page["items"])
            skipped.update(page["skipped"])
            cursor = page["next_cursor"]
        return items, skipped

    def cli_page(self, query):
        n = self.node
        result = subprocess.check_output([
            n.binaries / "psiosctl", "--socket", n.socket, "--token-file", n.witness / "owner.token",
            "task-result-eviction-candidates",
        ], input=json.dumps(query), text=True, timeout=20)
        return json.loads(result)

    def assert_items(self, items, *expected):
        self.assertEqual(items, [{"task_ref": row.task, "content_bytes": row.content_bytes} for row in expected])

    def assert_available(self, *observations):
        for item in observations:
            self.assertEqual(self.node.call("Task.Get", {"id": item.task["resource_id"]}), item.task_state)
            self.assertEqual(self.node.call("Artifact.Read", {"reference": item.artifact}), item.payload)

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertFalse(report["migration_required"], report)
        self.assertEqual(report["sqlite_integrity"], "Verified", report)

    def test_snapshot_rechecks_pins_and_changed_tasks_and_reaches_later_candidates(self):
        n = self.node
        first_item, new_item = self.create_project_pins()
        first = self.observe()
        other_owner = self.observe(owner=True)
        pinned = self.observe()
        new_pin = self.observe()
        removed = self.observe()
        later = self.observe()
        archived = self.observe()
        archived_ref = n.call("Task.Archive", target(archived.task), self.token)
        self.assertEqual(n.call("Resource.Resolve", {"reference": archived_ref})["value"]["task_ref"], archived.task)
        self.submit_pin(first_item, pinned)

        query = {"updated_before": timestamp(180), "principal_id": self.agent["principal_id"]}
        first_page = self.page(query, limit=1)
        self.assert_items(first_page["items"], first)
        cursor = first_page["next_cursor"]
        self.assertIsInstance(cursor, str, first_page)
        self.assertTrue(cursor, first_page)
        cli = self.cli_page({**query, "limit": 1})
        self.assertEqual(cli["items"], first_page["items"])
        resume = {**query, "cursor": cursor}
        self.denied("PermissionDenied", query, self.token)
        self.denied("PermissionDenied", resume, self.token)
        for wrong in (
            {**resume, "principal_id": "local-owner"},
            {"updated_before": query["updated_before"], "cursor": cursor},
            {**resume, "updated_before": timestamp(185)},
        ):
            self.denied("CursorExpired", wrong)
        middle = len(cursor) // 2
        changed = cursor[:middle] + ("A" if cursor[middle] != "A" else "B") + cursor[middle + 1:]
        self.denied("CursorExpired", {**resume, "cursor": changed})

        new_pending = self.submit_pin(new_item, new_pin)
        n.call("Task.EvictResults", {**target(removed.task), "reason": "independent removal before next page"})
        new_task = self.observe()
        n.stop(signal.SIGKILL)
        n.start()

        instant = datetime.fromisoformat(query["updated_before"])
        same_cutoff = instant.astimezone(timezone(timedelta(hours=5, minutes=30))).isoformat()
        rest, skipped = self.remainder({**query, "updated_before": same_cutoff}, cursor, limit=1)
        self.assert_items(rest, later, archived)
        self.assertEqual(dict(skipped), {"changed": 1, "ineligible": 0, "pinned": 2}, skipped)
        fresh = self.page(query, limit=16)
        self.assert_items(fresh["items"], first, later, archived, new_task)
        self.assertEqual(fresh["skipped"]["pinned"], 2, fresh)
        self.assertIsNone(fresh["next_cursor"], fresh)
        owned = self.page({**query, "principal_id": "local-owner"})
        self.assert_items(owned["items"], other_owner)
        self.assert_available(first, pinned, new_pin, later, archived, new_task, other_owner)
        self.assertEqual(n.call("WorkItem.Get", {"id": new_item["resource_id"]})["reference"], new_pending)

        replace_cursor = self.page(query, limit=1)["next_cursor"]
        self.grant(self.agent, ["task.read"], [self.profile["resource_id"]])
        self.denied("CursorExpired", {**query, "cursor": replace_cursor})

        reason = "reclaim exact results selected by the resumed snapshot"
        plan = n.call(PREPARE, {
            "task_refs": [item["task_ref"] for item in rest], "reason": reason,
            "max_content_bytes": sum(item["content_bytes"] for item in rest),
        })
        receipt = n.call(APPLY, {"plan_token": plan["plan_token"]})
        self.assertEqual(receipt["batch"]["content_bytes_removed"], str(later.content_bytes + archived.content_bytes))
        evictions = [n.call("Resource.Resolve", {"reference": ref})["value"]
                     for ref in receipt["batch"]["eviction_refs"]]
        self.assertCountEqual([row["previous_task_ref"] for row in evictions], [later.task, archived.task])
        for original in (later, archived):
            self.assertEqual(n.call("Task.Get", {"id": original.task["resource_id"]})["result"]["availability"],
                             "Evicted")
            with self.assertRaises(Failure) as read:
                n.call("Artifact.Read", {"reference": original.artifact})
            self.assertEqual(read.exception.code, "ResultEvicted", read.exception)
        after = self.page(query)
        self.assert_items(after["items"], first, new_task)
        self.assert_available(first, pinned, new_pin, new_task, other_owner)
        self.verify()

    def test_cutoff_and_exact_owner_do_not_surface_successful_write_results(self):
        n = self.node
        observed = self.observe(owner=True)
        storage_task = n.submit("volume.create", {"name": "candidate-discovery-write"})
        settled = n.wait(storage_task, autoapprove=True)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"), settled)
        storage_output, = settled["result"]["artifact_refs"]
        query = {"updated_before": timestamp(30), "principal_id": "local-owner"}
        selected = self.page(query)
        self.assert_items(selected["items"], observed)
        self.assertEqual(selected["skipped"]["ineligible"], 1, selected)
        self.assertIsNone(selected["next_cursor"], selected)
        self.assertEqual(n.call("Artifact.Get", {"reference": storage_output})["availability"], "Available")

        cutoff = n.get(observed.task["resource_id"])["resource"]["metadata"]["updated_at"]
        for updated_before in ("2000-01-01T00:00:00Z", cutoff):
            previous = self.page({**query, "updated_before": updated_before})
            self.assertEqual(previous["items"], [], previous)
            self.assertEqual(previous["skipped"]["ineligible"], 2, previous)
            self.assertIsNone(previous["next_cursor"], previous)
        for invalid in (
            {**query, "updated_before": "2000-01-01T00:00:00"},
            {**query, "limit": 0},
            {**query, "limit": 17},
        ):
            self.denied("InvalidArgument", invalid)
        self.verify()


if __name__ == "__main__":
    unittest.main()
