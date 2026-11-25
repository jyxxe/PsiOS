"""Exercise the real local authority when its configured Docker Engine is unavailable."""
import json
import os
import subprocess
import unittest

from oci_capture import MAX, TOOL, assert_failed, denied, grant, no_lost_publication
from support import Node, key, target


class LocalCaptureAdmission(unittest.TestCase):
    def test_capture_ceiling_is_approved_exactly_and_rechecked_at_claim(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        node.stop()
        config = json.loads(node.config.read_text())
        config["docker_socket"] = str(node.base / "absent-engine.sock")
        node.config.write_text(json.dumps(config))
        node.start()

        owner = "local-owner"
        grant(node, owner, "artifact.upload")
        scope = grant(node, owner, "task.submit", "task.read", TOOL)["delegation_ref"]["resource_id"]
        capture = {"image_id": "sha256:" + "1" * 64, "expected_engine_id": "absent-engine-id"}
        for invalid in (0, 4 * MAX + 1, None):
            denied(node, "InvalidArgument", "Task.Prepare", {"tool": TOOL, "key": key(),
                "input": dict(capture, max_archive_bytes=invalid), "delegation_id": scope})
        capture["max_archive_bytes"] = 2 * MAX
        ready = node.prepare(TOOL, capture, delegation=scope)
        denied(node, "QuotaExceeded", "Task.Submit", ready)

        node.stop()
        node.config.write_text(json.dumps(dict(config, oci_capture_max_bytes=4 * MAX + 1)))
        invalid = subprocess.run(node.daemon + ["verify"], capture_output=True, timeout=10)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn(b"InvalidArgument", invalid.stderr)
        node.config.write_text(json.dumps(dict(config, oci_capture_max_bytes=2 * MAX)))
        node.start()
        self.assertEqual(node.call("Artifact.UploadProfile", {})["max_bytes"], MAX)
        denied(node, "QuotaExceeded", "Artifact.Stage", {"key": key(), "size_bytes": MAX + 1,
            "ttl_seconds": 60, "declared_media_type": "application/octet-stream", "digest": capture["image_id"]})
        task = node.call("Task.Submit", ready)["task_id"]
        waiting = node.wait(task)
        self.assertEqual(waiting["lifecycle"], "WaitingApproval")
        approval_ref = node.get(waiting["conditions"][0]["message"])["value"]["plan_ref"]
        plan = node.call("Resource.Resolve", {"reference": approval_ref})["value"]
        self.assertEqual(node.call("Resource.Resolve", {"reference": plan["purpose_ref"]})["value"], capture)

        node.stop()
        node.config.write_text(json.dumps(config))
        node.start()
        node.approve(task)
        settled = node.wait(task)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"], settled["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "QuotaExceeded"))
        self.assertEqual(settled["result"]["artifact_refs"], [])
        no_lost_publication(node, 0)

    def test_approval_and_capture_authority_survive_restart(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        node.stop()
        config = json.loads(node.config.read_text())
        node.config.write_text(json.dumps(dict(config, docker_socket=str(node.base / "absent-engine.sock"))))
        node.start()

        owner = "local-owner"
        scoped = grant(node, owner, "task.submit", "task.read", TOOL)
        delegation = scoped["delegation_ref"]["resource_id"]
        request = {"image_id": "sha256:" + "1" * 64, "expected_engine_id": "absent-engine-id"}
        prepared = node.prepare(TOOL, request, delegation=delegation)
        denied(node, "PermissionDenied", "Task.Submit", prepared)
        upload = grant(node, owner, "artifact.upload")
        first = node.call("Task.Submit", prepared)["task_id"]
        self.assertEqual(node.wait(first)["lifecycle"], "WaitingApproval")
        no_lost_publication(node, 0)

        node.stop()
        node.start()
        replay = node.call("Task.Submit", prepared)
        self.assertEqual((replay["task_id"], replay["deduplicated"]), (first, True))
        self.assertEqual(node.wait(first)["lifecycle"], "WaitingApproval")
        assert_failed(node.wait(first, autoapprove=True), "DependencyUnavailable")

        later = node.call("Task.Submit", node.prepare(TOOL, request, delegation=delegation))["task_id"]
        self.assertEqual(node.wait(later)["lifecycle"], "WaitingApproval")
        node.call("Grant.Revoke", target(upload["grant_ref"]))
        node.approve(later)
        settled = node.wait(later)
        self.assertEqual(settled["lifecycle"], "Failed")
        self.assertEqual(settled["conditions"][0]["type"], "PermissionDenied")
        self.assertEqual(settled["result"]["artifact_refs"], [])
        no_lost_publication(node, 0)
