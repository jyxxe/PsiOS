"""Exercise durable opaque artifact upload through the local socket and SDK CLI."""
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target


CHUNK = 128 * 1024
MAX = 8 * 1024 * 1024
MEDIA = "application/octet-stream"


def digest(payload):
    return "sha256:" + hashlib.sha256(payload).hexdigest()


def upload_request(payload, **extra):
    return {"key": key(), "declared_media_type": MEDIA,
            "digest": digest(payload), "size_bytes": len(payload), "ttl_seconds": 600, **extra}


def chunk(stage, offset, data):
    return {"stage_id": stage["stage_id"], "offset": offset, "data_base64": base64.b64encode(data).decode()}


class ArtifactIngressTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.node_id = self.n.call("Node.Get", {})["reference"]["resource_id"]

    def grant(self, actor, *actions):
        return self.n.call("Grant.Issue", {"principal_id": actor, "actions": actions, "units": 10,
                           "ttl_seconds": 120, "tool_targets": [self.node_id]})

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as denied:
            self.n.call(method, params, token)
        self.assertEqual(denied.exception.code, code, denied.exception)

    def test_authorized_stage_resume_and_independent_read_and_withdraw(self):
        n = self.n
        agent = n.call("Agent.Register", {"name": "artifact-author"})
        token = agent["token"]
        principal = agent["principal_id"]
        self.denied("PermissionDenied", "Artifact.UploadProfile", {}, token)
        first_grant = self.grant(principal, "artifact.upload")
        profile = n.call("Artifact.UploadProfile", {}, token)
        self.assertEqual((profile["format"], profile["chunk_bytes"], profile["max_bytes"]),
                         ("OpaqueBytesV1", CHUNK, MAX))
        payload = (bytes(range(256)) * 512) + b"\x00\xfftail"
        request = upload_request(payload, declared_media_type="text/html")
        with ThreadPoolExecutor(max_workers=2) as callers:
            results = list(callers.map(lambda _: n.call("Artifact.Stage", request, token), range(2)))
        original, duplicate = results
        self.assertEqual(original, duplicate)
        self.assertIsNone(original["artifact_ref"])
        self.assertEqual(n.call("Resource.List", {"kind": "Artifact"})["items"], [])
        self.denied("IdempotencyConflict", "Artifact.Stage", {**request, "declared_media_type": MEDIA}, token)
        second = n.call("Agent.Register", {"name": "artifact-outsider"})
        self.grant(second["principal_id"], "artifact.upload", "artifact.read")
        lookup = {"stage_id": original["stage_id"]}
        self.denied("NotFound", "Artifact.Stage.Get", lookup, second["token"])
        self.denied("PreconditionFailed", "Artifact.Publish", lookup, token)
        self.denied("PreconditionFailed", "Artifact.Stage.PutChunk", chunk(original, CHUNK, payload[CHUNK:]), token)
        part = chunk(original, 0, payload[:CHUNK])
        saved = n.call("Artifact.Stage.PutChunk", part, token)
        self.assertEqual(saved["received_bytes"], CHUNK)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Artifact.Stage.Get", lookup, token), saved)
        self.assertEqual(n.call("Artifact.Stage.PutChunk", part, token), saved)
        self.denied("IdempotencyConflict", "Artifact.Stage.PutChunk", chunk(original, 0, b"x" * CHUNK), token)

        n.call("Grant.Revoke", target(first_grant["grant_ref"]))
        self.denied("PermissionDenied", "Artifact.Stage.Get", lookup, token)
        self.grant(principal, "artifact.upload")
        n.call("Artifact.Stage.PutChunk", chunk(original, CHUNK, payload[CHUNK:]), token)
        n.stop()
        with sqlite3.connect(n.state / "state.db") as conn:
            conn.executescript("""CREATE TRIGGER fail_upload_publish BEFORE INSERT ON artifact_chunks
                                  BEGIN SELECT RAISE(ABORT, 'injected upload publication loss'); END;""")
        n.start()
        self.denied("StorageUnavailable", "Artifact.Publish", lookup, token)
        self.assertEqual(n.call("Artifact.Stage.Get", lookup, token)["received_bytes"], len(payload))
        self.assertEqual(n.call("Resource.List", {"kind": "Artifact"})["items"], [])
        n.stop()
        with sqlite3.connect(n.state / "state.db") as conn:
            conn.execute("DROP TRIGGER fail_upload_publish")
        n.start()
        published = n.call("Artifact.Publish", lookup, token)
        self.assertEqual(published["state"], "Published")
        self.assertEqual(n.call("Artifact.Publish", lookup, token), published)
        self.assertEqual(n.call("Artifact.Stage", request, token), published)
        ref = {"reference": published["artifact_ref"]}
        self.assertEqual(ref["reference"]["digest"], digest(payload))
        self.denied("PermissionDenied", "Artifact.Get", ref, token)
        self.denied("PermissionDenied", "Artifact.Get", ref, second["token"])

        self.grant(principal, "artifact.read")
        metadata = n.call("Artifact.Get", ref, token)
        spec = metadata["resource"]["spec"]
        self.assertEqual((spec["media_type"], spec["declared_media_type"], spec["source"]["principal_id"]),
                         (MEDIA, "text/html", principal))
        fetched = n.call("Artifact.ReadChunk", {**ref, "offset": CHUNK - 4, "length": 8}, token)
        self.assertEqual(base64.b64decode(fetched["data_base64"]), payload[CHUNK - 4:CHUNK + 4])
        self.denied("UnsupportedCapability", "Artifact.Read", ref, token)
        release = {**ref, "reason": "retire published fixture"}
        self.denied("PermissionDenied", "Artifact.Withdraw", release, token)
        self.grant(principal, "artifact.withdraw")
        withdrawn = n.call("Artifact.Withdraw", release, token)
        self.assertEqual(n.call("Artifact.Withdraw", release, token), withdrawn)
        self.denied("IdempotencyConflict", "Artifact.Withdraw", {**release, "reason": "other reason"}, token)
        self.denied("ResultEvicted", "Artifact.ReadChunk", {**ref, "offset": 0, "length": 1}, token)
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)
        n.start()
        self.assertEqual(n.call("Artifact.Get", ref, token)["availability"], "Evicted")
        self.assertEqual(n.call("Artifact.Stage", request, token)["state"], "Withdrawn")

    def test_quota_discard_expiry_and_digest_rejection(self):
        n = self.n
        self.grant("local-owner", "artifact.upload")
        full = [n.call("Artifact.Stage", upload_request(b"known", size_bytes=MAX)) for _ in range(3)]
        partial = b"p" * CHUNK
        expiry_request = upload_request(partial, size_bytes=MAX, ttl_seconds=5)
        expiry = n.call("Artifact.Stage", expiry_request)
        n.call("Artifact.Stage.PutChunk", chunk(expiry, 0, partial))
        self.denied("QuotaExceeded", "Artifact.Stage", upload_request(b"another"))
        released = n.call("Artifact.Stage.Discard", {"stage_id": full[0]["stage_id"]})
        self.assertEqual(released["state"], "Discarded")
        replacement = n.call("Artifact.Stage", upload_request(b"good"))
        n.call("Artifact.Stage.PutChunk", chunk(replacement, 0, b"bad!"))
        self.denied("PreconditionFailed", "Artifact.Publish", {"stage_id": replacement["stage_id"]})
        self.assertEqual(n.call("Resource.List", {"kind": "Artifact"})["items"], [])
        until = time.monotonic() + 9
        with sqlite3.connect(n.state / "state.db") as conn:
            while time.monotonic() < until:
                state, = conn.execute("SELECT state FROM artifact_uploads WHERE id=?", (expiry["stage_id"],)).fetchone()
                if state == "Expired":
                    break
                time.sleep(0.1)
            else:
                self.fail("daemon did not reclaim an expired stage")
            self.assertEqual(conn.execute("SELECT count(*) FROM artifact_upload_chunks WHERE upload_id=?",
                                          (expiry["stage_id"],)).fetchone(), (0,))
        self.assertEqual(n.call("Artifact.Stage", expiry_request)["state"], "Expired")
        self.denied("DeadlineExceeded", "Artifact.Publish", {"stage_id": expiry["stage_id"]})
        self.assertEqual(n.call("Artifact.Stage", upload_request(b"relieved"))["state"], "Open")

    def test_sdk_resumes_a_staged_file_and_cli_downloads_original_bytes(self):
        n = self.n
        self.grant("local-owner", "artifact.upload", "artifact.read")
        payload = b"\x87" * CHUNK + b"\x00\xff\x11"
        source = n.base / "bundle.bin"
        source.write_bytes(payload)
        req = upload_request(payload)
        stage = n.call("Artifact.Stage", req)
        n.call("Artifact.Stage.PutChunk", chunk(stage, 0, payload[:CHUNK]))
        cmd = ("artifact-upload", source, "--key", req["key"])
        receipt = n.cli(n.witness / "owner.token", *cmd)
        self.assertEqual(receipt["stage_id"], stage["stage_id"])
        self.assertEqual(n.cli(n.witness / "owner.token", *cmd), receipt)
        recovered = subprocess.check_output([n.binaries / "psiosctl", "--config", n.config, "artifact"],
                                            input=json.dumps(receipt["reference"]).encode(), timeout=15)
        self.assertEqual(recovered, payload)

    def test_offline_verify_rejects_damaged_staged_payload(self):
        n = self.n
        self.grant("local-owner", "artifact.upload")
        payload = b"durable upload evidence"
        stage = n.call("Artifact.Stage", upload_request(payload))
        n.call("Artifact.Stage.PutChunk", chunk(stage, 0, payload))
        n.stop()
        with sqlite3.connect(n.state / "state.db") as conn:
            conn.execute("UPDATE artifact_upload_chunks SET payload=zeroblob(length(payload)) WHERE upload_id=?",
                         (stage["stage_id"],))
        check = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(check.returncode, 0)
        self.assertIn("staged artifact chunk digest mismatch", check.stderr)
