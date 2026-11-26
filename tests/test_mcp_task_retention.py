"""MCP handles keep exact ordinary Task results available for their lifetime."""
from datetime import datetime, timedelta, timezone
import time
import unittest

from support import Failure, key, target
from test_mcp_tasks import McpTaskFixture


class McpTaskRetentionTests(McpTaskFixture, unittest.TestCase):
    def test_handle_pins_exact_payload_until_expiry(self):
        node = self.node
        grant = self.grant([self.profile], units=1)
        client = self.task_wire(grant, ttl=8)
        handle = self.begin(client, key())["taskId"]
        done = self.wait_status(client, handle)
        receipt = self.check_receipt(done["result"], "system.status", grant)
        task_id = receipt["task_id"]
        task = node.get(task_id)["reference"]
        [artifact] = node.call("Task.Results", {"id": task_id})["result"]["artifact_refs"]
        single = {**target(task), "reason": "Reclaim the expired tool result"}
        batch = {"task_refs": [task], "reason": single["reason"]}
        candidates = {
            "principal_id": self.agent["principal_id"],
            "updated_before": (datetime.now(timezone.utc) + timedelta(minutes=1)).isoformat(),
        }

        for method, params in (("Task.EvictResults", single),
                               ("Task.PrepareResultEvictionBatch", batch)):
            with self.assertRaises(Failure) as denied:
                node.call(method, params)
            self.assertEqual(denied.exception.code, "Conflict")
        pinned = node.call("Task.ResultEvictionCandidates", candidates)
        self.assertEqual(pinned["items"], [])
        self.assertEqual(pinned["skipped"]["pinned"], 1)
        self.assertEqual(node.call("Artifact.Read", {"reference": artifact}), receipt["result"])

        deadline = time.monotonic() + 12
        while True:
            frame = self.task_call(client, "tasks/get", {"taskId": handle})
            if "error" in frame:
                self.assertEqual(frame["error"]["code"], -32602, frame)
                break
            self.assertEqual(frame["result"]["result"]["structuredContent"], receipt)
            self.assertLess(time.monotonic(), deadline, "MCP result lifetime never expired")
            time.sleep(0.25)

        available = node.call("Task.ResultEvictionCandidates", candidates)
        self.assertEqual([item["task_ref"] for item in available["items"]], [task])
        plan = node.call("Task.PrepareResultEvictionBatch", batch)
        node.call("Task.ApplyResultEvictionBatch", {"plan_token": plan["plan_token"]})
        settled = node.call("Task.Results", {"id": task_id})
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"))
        self.assertEqual(settled["result"]["availability"], "Evicted")
        with self.assertRaises(Failure) as gone:
            node.call("Artifact.Read", {"reference": artifact})
        self.assertEqual(gone.exception.code, "ResultEvicted")
