"""OCI restore CLI prepares the existing Task API requests over the Unix socket."""
from datetime import datetime, timedelta, timezone
import json
import os
import subprocess
import unittest

from support import Failure, Node, target
from test_artifact_install import grant


class OciRestoreCli(unittest.TestCase):
    def test_prepares_restore_and_inspection_without_admitting_a_task(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        restore = "application.restore_release_image"
        inspect = "application.inspect_release_image_restore"
        permit = grant(node, "local-owner", ["task.submit", "task.read", restore, inspect], [node_id])
        delegation = permit["delegation_ref"]["resource_id"]
        deadline = (datetime.now(timezone.utc) + timedelta(seconds=40)).isoformat()
        token_file = node.witness / "owner.token"

        def reference(kind, digit):
            return {"resource_id": f"{kind}-fixture", "version": "r3", "digest": "sha256:" + digit * 64}

        application = reference("app", "1")
        operation = reference("op", "5")
        exact = {"release_ref": reference("release", "2"), "image_archive_ref": reference("artifact", "3"),
                 "publisher_key_ref": reference("oci-key", "4"), "expected_engine_id": "fixture-engine-id"}
        cases = [
            ("application-prepare-image-restore", restore, {"application_ref": application, **exact},
             {"target": target(application), **exact}),
            ("application-prepare-image-restore-inspection", inspect, operation, target(operation)),
        ]
        for command, action, content, expected in cases:
            with self.subTest(action=action):
                path = node.base / "restore-request.json"
                path.write_text(json.dumps(content))
                arguments = [command, "--key", "prepare-only", "--delegation", delegation,
                             "--deadline-at", deadline, path]
                request = node.cli(token_file, *arguments)
                self.assertEqual((request["task_type"], request["idempotency_key"], request["deadline_at"]),
                                 (action, "prepare-only", deadline))
                self.assertEqual(request["delegation_ref"], permit["delegation_ref"])
                self.assertEqual(node.call("Resource.Resolve", {"reference": request["input_ref"]})["value"], expected)
                self.assertEqual(node.call("Task.LookupSubmission", request)["status"], "AbsentWithinWindow")
                with self.assertRaises(Failure) as absent:
                    node.call("Task.Submit", request)
                self.assertEqual(absent.exception.code, "NotFound")

                path.write_text(json.dumps(dict(content, unknown_field=True)))
                invalid = subprocess.run([node.binaries / "psiosctl", "--socket", node.socket,
                                          "--token-file", token_file, *arguments], capture_output=True, timeout=5)
                self.assertNotEqual(invalid.returncode, 0)
                self.assertEqual(invalid.stdout, b"")
                self.assertIn(b"unknown field", invalid.stderr)
