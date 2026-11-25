"""Exercise the real local authority when its configured Docker Engine is unavailable."""
import json
import os
import unittest

from oci_capture import TOOL, assert_failed, denied, grant, no_lost_publication
from support import Node, target


class LocalCaptureAdmission(unittest.TestCase):
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
