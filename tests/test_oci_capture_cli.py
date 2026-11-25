"""Prepare approved OCI capture requests through the Unix CLI and Rust SDK."""
from datetime import datetime, timedelta, timezone
import json
import os
import subprocess
import unittest

from support import Failure, Node
from test_artifact_install import grant


class OciCaptureCli(unittest.TestCase):
    def test_exact_optional_limit_and_no_implicit_admission(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        node.stop()
        config = json.loads(node.config.read_text())
        node.config.write_text(json.dumps(dict(config, oci_capture_max_bytes=16 * 1024 * 1024)))
        node.start()
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        tool = "application.capture_oci_image"
        permit = grant(node, "local-owner", ["task.submit", "task.read", tool], [node_id])
        deadline = (datetime.now(timezone.utc) + timedelta(seconds=40)).strftime("%Y-%m-%dT%H:%M:%SZ")
        digest = "sha256:" + "6" * 64
        engine = "fixture-docker-engine"
        token_file = node.witness / "owner.token"
        base = ["application-prepare-image-capture", "--image-id", digest, "--engine-id", engine,
                "--delegation", permit["delegation_ref"]["resource_id"], "--deadline-at", deadline]
        for limit in (None, 12 * 1024 * 1024):
            with self.subTest(limit=limit):
                request_id = "capture-default" if limit is None else "capture-explicit"
                flags = ["--max-archive-bytes", str(limit)] if limit is not None else []
                request = node.cli(token_file, *base, "--key", request_id, *flags)
                self.assertEqual((request["task_type"], request["idempotency_key"], request["deadline_at"]),
                                 (tool, request_id, deadline))
                self.assertEqual(request["delegation_ref"], permit["delegation_ref"])
                expected = {"image_id": digest, "expected_engine_id": engine}
                if limit is not None:
                    expected["max_archive_bytes"] = limit
                self.assertEqual(node.call("Resource.Resolve", {"reference": request["input_ref"]})["value"], expected)
                self.assertEqual(node.call("Task.LookupSubmission", request)["status"], "AbsentWithinWindow")
                with self.assertRaises(Failure) as denied:
                    node.call("Task.Submit", request)
                self.assertEqual(denied.exception.code, "PermissionDenied")

        invalid = subprocess.run([node.binaries / "psiosctl", "--socket", node.socket,
                                  "--token-file", token_file, *base, "--key", "capture-too-large",
                                  "--max-archive-bytes", str(32 * 1024 * 1024 + 1)],
                                 capture_output=True, timeout=5)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertEqual(invalid.stdout, b"")
        self.assertIn(b"InvalidArgument", invalid.stderr)

        grant(node, "local-owner", ["artifact.upload"], [node_id])
        admitted = node.call("Task.Submit", request)
        waiting = node.wait(admitted["task_id"])
        self.assertEqual(waiting["lifecycle"], "WaitingApproval")
        self.assertEqual(waiting["result"]["artifact_refs"], [])
