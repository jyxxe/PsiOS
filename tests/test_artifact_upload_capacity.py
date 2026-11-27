"""Observe and enforce durable upload receipt limits through the Unix API."""

from concurrent.futures import ThreadPoolExecutor
import json
import subprocess
import unittest

from support import Failure, Node
from test_artifact_ingress import CHUNK, chunk, upload_request
from test_artifact_install import grant


class ArtifactUploadCapacityTests(unittest.TestCase):
    def setUp(self):
        self.n = Node()
        self.addCleanup(self.n.close)
        self.settings = json.loads(self.n.config.read_text())
        self.node_id = self.n.call("Node.Get", {})["reference"]["resource_id"]

    def configure(self, per_principal, node):
        self.n.stop()
        self.n.config.write_text(json.dumps({**self.settings, "artifact_upload_history": {
            "per_principal": per_principal, "node": node,
        }}))
        self.n.start()

    def actor(self, name, actions):
        actor = self.n.call("Agent.Register", {"name": name})
        grant(self.n, actor["principal_id"], actions, [self.node_id])
        return actor["token"]

    def capacity(self, token=None):
        return self.n.call("Artifact.UploadCapacity", {}, token)

    def deny(self, code, method, request, token):
        with self.assertRaises(Failure) as denied:
            self.n.call(method, request, token)
        self.assertEqual(denied.exception.code, code, denied.exception)
        return denied.exception.args[0]

    def test_byte_charges_and_receipts_are_visible_only_at_the_authorized_scope(self):
        n = self.n
        actor = self.actor("artifact-charge-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"])
        other = self.actor("artifact-charge-peer", ["artifact.upload"])
        self.deny("PermissionDenied", "Artifact.UploadCapacity", {},
                  self.actor("artifact-read-only", ["artifact.read"]))
        profile = n.call("Artifact.UploadProfile", {}, actor)
        self.assertEqual((profile["owner_receipts"], profile["total_receipts"]), (4096, 65536))
        payload = b"x" * CHUNK + b"last"
        request = upload_request(payload)
        staged = n.call("Artifact.Stage", request, actor)
        n.call("Artifact.Stage.PutChunk", chunk(staged, 0, payload[:CHUNK]), actor)
        measured = self.capacity(actor)
        self.assertEqual(set(measured), {"principal"})
        self.assertEqual(measured["principal"], {
            "reserved_bytes": len(payload), "retained_bytes": 0, "open_stages": 1, "receipts": 1,
            "byte_limit": 32 * 1024 * 1024, "open_stage_limit": 4, "receipt_limit": 4096,
            "remaining_receipts": 4095,
            "oci_archive_retained_bytes": 0, "oci_archive_byte_limit": 32 * 1024 * 1024,
            "oci_archive_pending_disposal_bytes": 0,
        })
        peer = self.capacity(other)
        self.assertEqual(set(peer), {"principal"})
        self.assertEqual(peer["principal"]["receipts"], 0)
        global_view = self.capacity()
        self.assertEqual(global_view["principal"]["receipts"], 0)
        self.assertEqual(global_view["node"]["reserved_bytes"], len(payload))
        self.assertEqual(global_view["node"]["remaining_receipts"], 65535)
        tokenfile = n.base / "upload.token"
        tokenfile.write_text(actor)
        tokenfile.chmod(0o600)
        self.assertEqual(n.cli(tokenfile, "artifact-upload-capacity"), measured)

        n.call("Artifact.Stage.PutChunk", chunk(staged, CHUNK, payload[CHUNK:]), actor)
        published = n.call("Artifact.Publish", {"stage_id": staged["stage_id"]}, actor)
        usage = self.capacity(actor)["principal"]
        self.assertEqual((usage["reserved_bytes"], usage["retained_bytes"], usage["open_stages"], usage["receipts"]),
                         (0, len(payload), 0, 1))
        n.call("Artifact.Withdraw", {"reference": published["artifact_ref"], "reason": "release durable charge"}, actor)
        usage = self.capacity(actor)["principal"]
        self.assertEqual((usage["retained_bytes"], usage["receipts"], usage["remaining_receipts"]), (0, 1, 4095))
        replay = n.call("Artifact.Stage", request, actor)
        self.assertEqual((replay["stage_id"], replay["state"], replay["artifact_ref"]),
                         (staged["stage_id"], "Withdrawn", published["artifact_ref"]))

    def test_configured_receipt_capacity_is_atomic_and_preserves_settlement_and_replays(self):
        n = self.n
        self.configure(2, 3)
        first = self.actor("receipt-owner", ["artifact.upload"])
        second = self.actor("receipt-peer-a", ["artifact.upload"])
        third = self.actor("receipt-peer-b", ["artifact.upload"])
        profile = n.call("Artifact.UploadProfile", {}, first)
        self.assertEqual((profile["owner_receipts"], profile["total_receipts"]), (2, 3))
        stable = upload_request(b"old history")
        for request in (stable, upload_request(b"second history")):
            staged = n.call("Artifact.Stage", request, first)
            n.call("Artifact.Stage.Discard", {"stage_id": staged["stage_id"]}, first)
        denial = self.deny("QuotaExceeded", "Artifact.Stage", upload_request(b"full principal"), first)
        self.assertEqual(denial["retry_condition"], "Never")
        self.assertEqual(self.capacity(first)["principal"]["remaining_receipts"], 0)
        self.assertEqual(n.call("Artifact.Stage", stable, first)["state"], "Discarded")

        def reserve(token):
            request = upload_request(b"one slot")
            try:
                return token, request, n.call("Artifact.Stage", request, token)
            except Failure as error:
                self.assertEqual(error.code, "QuotaExceeded", error)
                return token, request, None

        with ThreadPoolExecutor(max_workers=2) as threads:
            results = list(threads.map(reserve, [second, third]))
        accepted, = [result for result in results if result[2] is not None]
        rejected, = [result for result in results if result[2] is None]
        self.assertEqual(self.capacity()["node"]["receipts"], 3)
        self.assertEqual(self.capacity()["node"]["remaining_receipts"], 0)

        token, request, stage = accepted
        self.configure(1, 2)
        self.assertEqual(self.capacity()["node"]["receipts"], 3)
        self.assertEqual(self.capacity()["node"]["remaining_receipts"], 0)
        self.assertEqual(n.call("Artifact.Stage", request, token), stage)
        n.call("Artifact.Stage.PutChunk", chunk(stage, 0, b"one slot"), token)
        published = n.call("Artifact.Publish", {"stage_id": stage["stage_id"]}, token)
        self.assertEqual(self.capacity(token)["principal"]["retained_bytes"], len(b"one slot"))

        self.configure(3, 4)
        self.assertEqual(self.capacity()["node"]["remaining_receipts"], 1)
        self.assertEqual(n.call("Artifact.Stage", request, token)["artifact_ref"], published["artifact_ref"])
        self.deny("IdempotencyConflict", "Artifact.Stage", {**upload_request(b"changed"), "key": stable["key"]}, first)
        retry_token, retry_request, _ = rejected
        later = n.call("Artifact.Stage", retry_request, retry_token)
        self.assertEqual(later["state"], "Open")
        self.assertEqual(self.capacity()["node"]["remaining_receipts"], 0)

    def test_invalid_history_limits_fail_configuration_check(self):
        self.n.stop()
        for per_principal, node in [(0, 1), (3, 2), (4097, 65536), (1, 65537)]:
            with self.subTest(per_principal=per_principal, node=node):
                self.n.config.write_text(json.dumps({**self.settings, "artifact_upload_history": {
                    "per_principal": per_principal, "node": node,
                }}))
                checked = subprocess.run(self.n.daemon + ["check"], capture_output=True, text=True, timeout=15)
                self.assertNotEqual(checked.returncode, 0)
                self.assertIn("invalid artifact upload history limits", checked.stderr)

    def test_archive_profile_rejects_a_capture_that_cannot_fit_its_limits(self):
        self.n.stop()
        mib = 1024 * 1024
        profile = {"oci_capture_max_bytes": 64 * mib, "oci_capture_staging_max_bytes": 96 * mib,
                   "oci_archive_storage": {"principal_bytes": 128 * mib, "node_bytes": 256 * mib}}
        cases = [
            ({"oci_capture_max_bytes": 129 * mib}, "OCI capture maximum"),
            ({"oci_capture_staging_max_bytes": 63 * mib}, "OCI staging capacity"),
            ({"oci_capture_staging_max_bytes": 513 * mib}, "OCI staging capacity"),
            ({"oci_archive_storage": {"principal_bytes": 63 * mib, "node_bytes": 256 * mib}}, "OCI retained capacity"),
            ({"oci_archive_storage": {"principal_bytes": 128 * mib, "node_bytes": 127 * mib}}, "OCI retained capacity"),
            ({"oci_archive_storage": {"principal_bytes": 128 * mib, "node_bytes": 2049 * mib}}, "OCI retained capacity"),
        ]
        for settings, message in cases:
            with self.subTest(settings=settings):
                self.n.config.write_text(json.dumps({**self.settings, **profile, **settings}))
                checked = subprocess.run(self.n.daemon + ["check"], capture_output=True, text=True, timeout=15)
                self.assertNotEqual(checked.returncode, 0, checked)
                self.assertIn(message, checked.stderr)
