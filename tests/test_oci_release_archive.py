"""Reject unsigned archive declarations, profile confusion and ordinary uploaded bytes."""
import json
import os
import unittest

from oci_capture import MEDIA as CAPTURE_MEDIA, denied
from support import Node, key
from test_artifact_ingress import chunk, upload_request
from test_artifact_install import MEDIA, TOOL, grant
from test_oci_publisher import local_signature


ARCHIVED_MEDIA = "application/vnd.psios.oci-install.v2+json"


def upload(node, payload, media):
    stage = node.call("Artifact.Stage", upload_request(payload, declared_media_type=media))
    node.call("Artifact.Stage.PutChunk", chunk(stage, 0, payload))
    return node.call("Artifact.Publish", {"stage_id": stage["stage_id"]})["artifact_ref"]


class OciArchiveAdmission(unittest.TestCase):
    def test_signature_scope_and_authenticated_capture_are_all_required(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        owner = "local-owner"
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        grant(node, owner, ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
        fake = upload(node, b"ordinary uploaded bytes", CAPTURE_MEDIA)
        install = {"name": "archive-check", "image_id": "sha256:" + "1" * 64,
                   "argv": ["/probe"], "release_version": "1.0.0", "license_expression": "0BSD",
                   "cpu_millicores": 200, "memory_max_bytes": 32 * 1024 * 1024,
                   "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024}
        declaration = {"format": "PsiOSLocalOciInstallV2", "install": install, "image_archive_ref": fake}
        source = upload(node, json.dumps(declaration).encode(), ARCHIVED_MEDIA)
        signature, _, _ = local_signature(node, source)
        key_ref = signature["key_ref"]
        actions = ["task.submit", "task.read", TOOL]
        scopes = ["profile-local", source["resource_id"], key_ref["resource_id"]]
        missing = grant(node, owner, actions, scopes)
        allowed = grant(node, owner, actions, [*scopes, fake["resource_id"]])
        base = {"reference": source, "key": key()}

        def prepare(code, delegation, **overrides):
            denied(node, code, "Application.PrepareArtifactInstall",
                   dict(base, delegation_id=delegation["delegation_ref"]["resource_id"], **overrides))

        prepare("PermissionDenied", allowed)
        prepare("PermissionDenied", missing, signature=signature)
        prepare("PreconditionFailed", allowed, signature=signature)
        direct = node.prepare(TOOL, {"reference": source, "signature": signature},
                              delegation=allowed["delegation_ref"]["resource_id"])
        denied(node, "PreconditionFailed", "Task.Submit", direct)

        crossed = upload(node, json.dumps({"format": "PsiOSLocalOciInstallV1", "install": install}).encode(), ARCHIVED_MEDIA)
        denied(node, "InvalidArgument", "Application.PrepareArtifactInstall",
               {"reference": crossed, "key": key(), "delegation_id": allowed["delegation_ref"]["resource_id"]})
        v1 = upload(node, json.dumps(declaration).encode(), MEDIA)
        denied(node, "InvalidArgument", "Application.PrepareArtifactInstall",
               {"reference": v1, "key": key(), "delegation_id": allowed["delegation_ref"]["resource_id"]})
        for artifact in (fake, source, crossed, v1):
            node.call("Artifact.Withdraw", {"reference": artifact, "reason": "no accepted archive admission"})
