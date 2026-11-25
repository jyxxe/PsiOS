"""Actual Ed25519 signing, scoped admission and durable key revocation through the Unix API."""
import base64
import json
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key
from test_artifact_install import TOOL, grant, publish_install


def local_signature(node, artifact):
    publisher = node.call("Agent.Register", {"name": "local-oci-publisher"})
    private = node.base / (key() + ".pem")
    subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", private], check=True)
    public = subprocess.check_output(["openssl", "pkey", "-in", private, "-pubout", "-outform", "DER"])
    prefix = bytes.fromhex("302a300506032b6570032100")
    assert public.startswith(prefix) and len(public) == len(prefix) + 32
    registration = {"principal_id": publisher["principal_id"],
                    "public_key_base64": base64.b64encode(public[len(prefix):]).decode()}
    enrolled = node.cli(node.witness / "owner.token", "artifact-publisher-key-register",
                        registration["principal_id"], "--public-key-base64", registration["public_key_base64"])
    specification = node.call("Artifact.Get", {"reference": artifact})["resource"]["spec"]
    statement = {"domain": "psios/local-oci-declaration-signature/1",
                 "artifact_digest": artifact["digest"], "declared_media_type": specification["declared_media_type"],
                 "size_bytes": specification["size_bytes"]}
    message = node.base / (key() + ".json")
    message.write_bytes(json.dumps(statement, sort_keys=True, separators=(",", ":")).encode())
    signed = subprocess.check_output(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", private, "-in", message])
    private.unlink()
    sign = {"key_ref": enrolled["reference"], "signature_base64": base64.b64encode(signed).decode()}
    return sign, registration, publisher


class OciPublisherTests(unittest.TestCase):
    def test_signature_scope_and_key_revocation_across_restart(self):
        n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(n.close)
        node_id = n.call("Node.Get", {})["reference"]["resource_id"]
        grant(n, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
        spec = {"name": "publisher-check", "image_id": "sha256:" + "1" * 64,
                "argv": ["/probe"], "release_version": "1.0.0", "license_expression": "0BSD",
                "cpu_millicores": 200, "memory_max_bytes": 32 * 1024 * 1024,
                "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024}
        artifact = publish_install(n, spec)
        changed = publish_install(n, {**spec, "release_version": "2.0.0"})
        proof, registration, publisher = local_signature(n, artifact)
        signer = proof["key_ref"]
        self.assertEqual(n.call("Artifact.PublisherKey.Register", registration)["reference"], signer)
        with self.assertRaises(Failure) as unauthorised:
            n.call("Artifact.PublisherKey.Register", registration, publisher["token"])
        self.assertEqual(unauthorised.exception.code, "PermissionDenied")

        actions = ["task.submit", "task.read", TOOL]
        old_scope = ["profile-local", artifact["resource_id"], changed["resource_id"]]
        source_only = grant(n, "local-owner", actions, old_scope)
        permitted = grant(n, "local-owner", actions, [*old_scope, signer["resource_id"]])

        def prepare(reference, delegation):
            return n.call("Application.PrepareArtifactInstall", {"reference": reference, "key": key(),
                "delegation_id": delegation["delegation_ref"]["resource_id"], "signature": proof})

        with self.assertRaises(Failure) as different:
            prepare(changed, permitted)
        self.assertEqual(different.exception.code, "PreconditionFailed")
        with self.assertRaises(Failure) as missing_scope:
            prepare(artifact, source_only)
        self.assertEqual(missing_scope.exception.code, "PermissionDenied")

        ref_file = n.base / "artifact-ref.json"
        ref_file.write_text(json.dumps(artifact))
        proof_file = n.base / "signature.json"
        proof_file.write_text(json.dumps(proof))
        ready = n.cli(n.witness / "owner.token", "artifact-prepare-install", "--delegation",
                      permitted["delegation_ref"]["resource_id"], "--key", key(),
                      "--signature", proof_file, ref_file)
        self.assertEqual(ready["publisher"]["source_trust"], "PublisherVerified")
        self.assertEqual(ready["publisher"]["identity_ref"]["resource_id"], publisher["principal_id"])
        self.assertEqual(ready["resource_refs"][1:], [artifact, signer])
        task_id = n.call("Task.Submit", ready["request"])["task_id"]
        pending = n.wait(task_id)
        self.assertEqual(pending["lifecycle"], "WaitingApproval")
        plan = n.get(pending["operation_refs"][0]["resource_id"])["value"]["plan_ref"]
        plan = n.call("Resource.Resolve", {"reference": plan})["value"]
        self.assertEqual([p["resource_ref"] for p in plan["preconditions"]], ready["resource_refs"])
        revoked = n.cli(n.witness / "owner.token", "artifact-publisher-key-revoke", signer["resource_id"],
                        "--revision", signer["version"], "--reason", "end of signing trust")
        self.assertNotEqual(revoked["version"], signer["version"])
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Artifact.PublisherKey.Register", registration)["lifecycle"], "Revoked")
        n.approve(task_id)
        denied = n.wait(task_id)
        self.assertEqual((denied["lifecycle"], denied["outcome_knowledge"], denied["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PermissionDenied"), denied)
        n.call("Artifact.Withdraw", {"reference": artifact, "reason": "revoked signed install"})
        self.assertEqual(n.call("Artifact.Get", {"reference": artifact})["availability"], "Evicted")
