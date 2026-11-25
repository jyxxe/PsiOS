"""Exercise OCI declaration preparation, pin fencing and recovery without a local engine."""
import json
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target
from test_artifact_ingress import chunk, upload_request


TOOL = "application.install_artifact"
MEDIA = "application/vnd.psios.oci-install.v1+json"


def publish_install(node, install, token=None, media=MEDIA):
    body = json.dumps({"format": "PsiOSLocalOciInstallV1", "install": install}).encode()
    staged = node.call("Artifact.Stage", upload_request(body, declared_media_type=media), token)
    node.call("Artifact.Stage.PutChunk", chunk(staged, 0, body), token)
    return node.call("Artifact.Publish", {"stage_id": staged["stage_id"]}, token)["artifact_ref"]


def grant(node, actor, actions, targets):
    return node.call("Grant.Issue", {"principal_id": actor, "actions": actions, "units": 8,
                     "ttl_seconds": 180, "tool_targets": targets})


def prepare(node, reference, permit, token=None):
    return node.call("Application.PrepareArtifactInstall", {"reference": reference,
                     "key": key(), "delegation_id": permit["delegation_ref"]["resource_id"]}, token)


class ArtifactInstallTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.node_id = self.node.call("Node.Get", {})["reference"]["resource_id"]
        self.spec = {"name": "artifact-app", "image_id": "sha256:" + "a" * 64,
                     "argv": ["/probe"], "release_version": "1.0.0", "license_expression": "0BSD",
                     "cpu_millicores": 200, "memory_max_bytes": 32 * 1024 * 1024,
                     "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024}

    def denied(self, code, action, *args):
        with self.assertRaises(Failure) as denied:
            action(*args)
        self.assertEqual(denied.exception.code, code, denied.exception)

    def test_revocation_and_reboot_preserve_the_pending_pin_until_known_failure(self):
        n = self.node
        agent = n.call("Agent.Register", {"name": "bundle-owner"})
        actor, token = agent["principal_id"], agent["token"]
        grant(n, actor, ["artifact.upload", "artifact.withdraw"], [self.node_id])
        reference = publish_install(n, self.spec, token)
        install = grant(n, actor, ["task.submit", "task.read", TOOL], ["profile-local", reference["resource_id"]])
        self.denied("PermissionDenied", prepare, n, reference, install, token)
        read = grant(n, actor, ["artifact.read"], [self.node_id])
        target_only = grant(n, actor, ["task.submit", "task.read", TOOL], ["profile-local"])
        self.denied("PermissionDenied", prepare, n, reference, target_only, token)
        planned = prepare(n, reference, install, token)
        self.assertEqual(planned["deployment"]["image_id"], self.spec["image_id"])
        self.assertEqual(planned["deployment"]["desired_state"], "Stopped")
        self.assertEqual(planned["resource_refs"][1], reference)
        self.assertEqual(n.call("Resource.List", {"kind": "Application"})["items"], [])
        accepted = n.call("Task.Submit", planned["request"], token)
        task_id = accepted["task_id"]
        waiting = n.wait(task_id)
        self.assertEqual(waiting["lifecycle"], "WaitingApproval", waiting)
        operation = n.get(waiting["operation_refs"][0]["resource_id"])["value"]
        plan = n.call("Resource.Resolve", {"reference": operation["plan_ref"]})["value"]
        self.assertEqual([p["resource_ref"] for p in plan["preconditions"]], planned["resource_refs"])
        self.assertEqual(plan["allowed_effects"][0]["action"], TOOL)
        release = {"reference": reference, "reason": "release after known failure"}
        self.denied("Conflict", n.call, "Artifact.Withdraw", release, token)
        n.stop(signal.SIGKILL)
        n.start()
        self.denied("Conflict", n.call, "Artifact.Withdraw", release, token)
        n.call("Grant.Revoke", target(read["grant_ref"]))
        n.approve(task_id)
        result = n.wait(task_id)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"], result["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PermissionDenied"), result)
        self.assertTrue(n.call("Artifact.Withdraw", release, token)["withdrawal_ref"])
        self.assertEqual(n.call("Resource.List", {"kind": "Application"})["items"], [])

    def test_unsupported_media_invalid_descriptor_and_unavailable_engine(self):
        n = self.node
        n.stop()
        config = json.loads(n.config.read_text())
        config["docker_socket"] = str(n.base / "missing-docker.sock")
        n.config.write_text(json.dumps(config))
        n.start()
        grant(n, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [self.node_id])
        wrong_media = publish_install(n, self.spec, media="application/octet-stream")
        invalid = publish_install(n, {**self.spec, "argv": ["relative-command"]})
        valid = publish_install(n, self.spec)
        permit = grant(n, "local-owner", ["task.submit", "task.read", TOOL],
                       ["profile-local", wrong_media["resource_id"], invalid["resource_id"], valid["resource_id"]])
        self.denied("UnsupportedCapability", prepare, n, wrong_media, permit)
        self.denied("InvalidArgument", prepare, n, invalid, permit)
        request = prepare(n, valid, permit)["request"]
        task = n.wait(n.call("Task.Submit", request)["task_id"], autoapprove=True)
        self.assertEqual(task["lifecycle"], "Failed", task)
        self.assertEqual(task["conditions"][0]["type"], "DependencyUnavailable", task)
        self.assertEqual(n.call("Resource.List", {"kind": "Application"})["items"], [])
        self.assertTrue(n.call("Artifact.Withdraw", {"reference": valid, "reason": "engine unavailable"})["withdrawal_ref"])

    def test_offline_verify_refuses_missing_pending_install_pin(self):
        n = self.node
        grant(n, "local-owner", ["artifact.upload", "artifact.read"], [self.node_id])
        ref = publish_install(n, self.spec)
        permit = grant(n, "local-owner", ["task.submit", "task.read", TOOL], ["profile-local", ref["resource_id"]])
        task_id = n.call("Task.Submit", prepare(n, ref, permit)["request"])["task_id"]
        self.assertEqual(n.wait(task_id)["lifecycle"], "WaitingApproval")
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)
        with sqlite3.connect(n.state / "state.db") as conn:
            self.assertEqual(conn.execute("DELETE FROM application_artifact_pins WHERE kind='Task' AND holder=?",
                                          (task_id,)).rowcount, 1)
        refused = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=15)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("application artifact retention pins disagree", refused.stderr)
