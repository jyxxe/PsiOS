"""Exercise exact, revocable upload-stage authority over the Unix API."""

import base64
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, target
from test_artifact_ingress import CHUNK, chunk, upload_request


BINARIES = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")
PRIOR_BINARIES = os.getenv("PSIOS_ARTIFACT_PRIOR_BIN_DIR")


class BoundArtifactUploadTests(unittest.TestCase):
    def start(self, binaries=BINARIES):
        self.node = Node(binaries)
        self.addCleanup(self.node.close)
        self.node_id = self.node.call("Node.Get", {})["reference"]["resource_id"]
        return self.node

    def grant(self, actor, *actions, resource=None):
        return self.node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": actions, "units": 4,
            "ttl_seconds": 600, "tool_targets": [resource or self.node_id],
        })

    def denied(self, expected, method, params, actor):
        with self.assertRaises(Failure) as caught:
            self.node.call(method, params, actor["token"])
        if isinstance(expected, str):
            expected = {expected}
        self.assertIn(caught.exception.code, expected, caught.exception)

    def stage(self, actor, grant, payload):
        req = upload_request(payload, delegation_ref=grant["delegation_ref"])
        view = self.node.call("Artifact.Stage", req, actor["token"])
        self.assertEqual(view["delegation_ref"], grant["delegation_ref"], view)
        self.assertEqual(view["state"], "Open", view)
        self.assertTrue(view["stage_id"].startswith("artifact-stage-bound"), view)
        return req, view, {"stage_id": view["stage_id"]}

    def current(self, actor, stage_id):
        return self.node.call("Artifact.Stage.Get", stage_id, actor["token"])

    def put(self, actor, stage, offset, data):
        return self.node.call("Artifact.Stage.PutChunk", chunk(stage, offset, data), actor["token"])

    def storage(self, stage_id):
        path = (self.node.state / "state.db").as_uri() + "?mode=ro"
        with closing(sqlite3.connect(path, uri=True, timeout=10)) as db:
            return db.execute("""SELECT u.state,
                (SELECT count(*) FROM artifact_upload_chunks WHERE upload_id=u.id),
                (SELECT count(*) FROM authorization_deadlines WHERE resource=u.id)
                FROM artifact_uploads u WHERE u.id=?""", (stage_id["stage_id"],)).fetchone()

    def reject_cleanup_commit(self, *stages):
        n = self.node
        n.stop()
        failure = "injected exact artifact authority cleanup failure"
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db, db:
            quoted = ",".join(db.execute("SELECT quote(?)", (stage["stage_id"],)).fetchone()[0]
                              for stage in stages)
            db.execute(f"""CREATE TRIGGER reject_artifact_authority_cleanup
                BEFORE UPDATE OF close_reason ON artifact_upload_authority
                WHEN NEW.close_reason='AuthorityRevoked' AND OLD.stage_id IN ({quoted})
                BEGIN SELECT RAISE(ABORT, '{failure}'); END""")
        errors = n.base / "cleanup-fault.log"
        with errors.open("wb") as stderr:
            n.process = subprocess.Popen([*n.daemon, "serve"], stderr=stderr)
        until = time.monotonic() + 10
        while True:
            self.assertIsNone(n.process.poll(), errors.read_text(errors="replace"))
            try:
                n.call("Principal.WhoAmI", {})
                break
            except (ConnectionError, FileNotFoundError):
                self.assertLess(time.monotonic(), until, "daemon did not start with fixture trigger")
                time.sleep(0.02)
        return errors, failure

    def reclaimed(self, actor, stage_id):
        deadline = time.monotonic() + 15
        while True:
            stored = self.storage(stage_id)
            if stored == ("Discarded", 0, 0):
                break
            self.assertLess(time.monotonic(), deadline, (stage_id, stored))
            time.sleep(0.05)
        view = self.current(actor, stage_id)
        self.assertEqual((view["state"], view["close_reason"], view["artifact_ref"]),
                         ("Discarded", "AuthorityRevoked", None), view)
        return view

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertFalse(report["migration_required"], report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"], report["witness"]),
                         ("Verified", "Verified", "Confirmed"), report)
        return report

    def test_revoked_exact_upload_loses_staging_but_preserves_durable_publication(self):
        n = self.start()
        actor = n.call("Agent.Register", {"name": "bound-uploader"})
        peer = n.call("Agent.Register", {"name": "unrelated-uploader"})
        first = self.grant(actor, "artifact.upload")
        keep = self.grant(actor, "artifact.upload")
        outside = self.grant(actor, "artifact.upload", resource="profile-local")
        self.grant(peer, "artifact.upload", "artifact.read")
        self.assertIs(n.call("Artifact.UploadProfile", {}, actor["token"])["exact_delegation"], True)
        self.denied("PermissionDenied", "Artifact.Stage",
                    upload_request(b"different subject", delegation_ref=first["delegation_ref"]), peer)
        self.denied("PermissionDenied", "Artifact.Stage",
                    upload_request(b"different target", delegation_ref=outside["delegation_ref"]), actor)
        forged_ref = {**first["delegation_ref"], "digest": "sha256:" + "0" * 64}
        self.denied("PermissionDenied", "Artifact.Stage", upload_request(b"wrong exact digest", delegation_ref=forged_ref), actor)

        published_bytes = b"\x00\xffpreviously published by original authority\x00"
        published_request, published_stage, published_id = self.stage(actor, first, published_bytes)
        self.put(actor, published_stage, 0, published_bytes)
        published = n.call("Artifact.Publish", published_id, actor["token"])
        self.assertEqual(published["state"], "Published", published)
        self.assertEqual(published["delegation_ref"], first["delegation_ref"])
        reference = {"reference": published["artifact_ref"]}
        self.denied("PermissionDenied", "Artifact.Get", reference, actor)
        self.grant(actor, "artifact.read")
        before = n.call("Artifact.Get", reference, actor["token"])

        first_bytes = b"a" * CHUNK + b"A original tail"
        ready_bytes = b"full but no publication yet"
        kept_bytes = b"b" * CHUNK + b"B live tail"
        first_req, first_stage, first_id = self.stage(actor, first, first_bytes)
        ready_req, ready_stage, ready_id = self.stage(actor, first, ready_bytes)
        _, kept_stage, kept_id = self.stage(actor, keep, kept_bytes)
        _, reserve_stage, reserve_id = self.stage(actor, keep, b"another live stage claims the fourth slot")
        self.assertEqual(reserve_stage["received_bytes"], 0)
        self.put(actor, first_stage, 0, first_bytes[:CHUNK])
        self.put(actor, ready_stage, 0, ready_bytes)
        original_offset = self.put(actor, kept_stage, 0, kept_bytes[:CHUNK])
        self.assertEqual(original_offset["received_bytes"], CHUNK)
        self.assertEqual(self.storage(first_id), ("Open", 1, 1))
        self.assertEqual(self.storage(ready_id), ("Open", 1, 1))
        self.denied("NotFound", "Artifact.Stage.Get", first_id, peer)
        self.denied("IdempotencyConflict", "Artifact.Stage", {**first_req, "delegation_ref": keep["delegation_ref"]}, actor)
        next_req = upload_request(b"test the freed slot", delegation_ref=keep["delegation_ref"])
        self.denied("QuotaExceeded", "Artifact.Stage", next_req, actor)

        errors, failure = self.reject_cleanup_commit(first_id, ready_id)
        n.call("Grant.Revoke", target(first["grant_ref"]))
        until = time.monotonic() + 15
        while failure not in errors.read_text(errors="replace"):
            self.assertLess(time.monotonic(), until, "the real reaper did not encounter its injected rollback")
            time.sleep(0.05)
        # The failed cleanup transaction preserved the rows and bytes. Exact
        # authority must already fence both mutation paths while the rows are open.
        self.assertEqual(self.storage(first_id), ("Open", 1, 1))
        self.assertEqual(self.storage(ready_id), ("Open", 1, 1))
        tail = chunk(first_stage, CHUNK, first_bytes[CHUNK:])
        self.denied("PermissionDenied", "Artifact.Stage.PutChunk", tail, actor)
        self.denied("PermissionDenied", "Artifact.Publish", ready_id, actor)
        self.denied("PermissionDenied", "Artifact.Stage", first_req, actor)
        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db, db:
            db.execute("DROP TRIGGER reject_artifact_authority_cleanup")
        n.start()
        first_closed = self.reclaimed(actor, first_id)
        ready_closed = self.reclaimed(actor, ready_id)
        self.assertEqual(first_closed["delegation_ref"], first["delegation_ref"])
        self.assertEqual(ready_closed["delegation_ref"], first["delegation_ref"])
        self.denied("PreconditionFailed", "Artifact.Stage.PutChunk", tail, actor)
        self.denied("PreconditionFailed", "Artifact.Publish", ready_id, actor)
        self.assertEqual(n.call("Artifact.Stage", first_req, actor["token"]), first_closed)
        self.assertEqual(n.call("Artifact.Stage", ready_req, actor["token"]), ready_closed)
        self.denied("IdempotencyConflict", "Artifact.Stage", {**first_req, "delegation_ref": keep["delegation_ref"]}, actor)

        self.assertEqual(self.current(actor, kept_id), original_offset)
        self.assertEqual(self.storage(kept_id), ("Open", 1, 1))
        self.assertEqual(self.storage(reserve_id), ("Open", 0, 1))
        self.assertEqual(n.call("Artifact.Stage", next_req, actor["token"])["state"], "Open")
        self.stage(actor, keep, b"also reclaim the second revoked reservation")
        self.denied("QuotaExceeded", "Artifact.Stage",
                    upload_request(b"quota still enforced", delegation_ref=keep["delegation_ref"]), actor)
        self.assertEqual(self.put(actor, kept_stage, CHUNK, kept_bytes[CHUNK:])["received_bytes"], len(kept_bytes))
        self.assertEqual(n.call("Artifact.Publish", kept_id, actor["token"])["state"], "Published")

        self.assertEqual(self.current(actor, published_id), published)
        self.assertEqual(n.call("Artifact.Stage", published_request, actor["token"]), published)
        self.assertEqual(n.call("Artifact.Publish", published_id, actor["token"]), published)
        self.assertEqual(n.call("Artifact.Get", reference, actor["token"]), before)
        read = {**reference, "offset": 0, "length": len(published_bytes)}
        self.assertEqual(base64.b64decode(n.call("Artifact.ReadChunk", read, actor["token"])["data_base64"]),
                         published_bytes)
        self.denied("PermissionDenied", "Artifact.ReadChunk", read, peer)
        withdrawal = {**reference, "reason": "withdraw after independent authorization"}
        self.denied("PermissionDenied", "Artifact.Withdraw", withdrawal, actor)
        self.grant(actor, "artifact.withdraw")
        n.call("Artifact.Withdraw", withdrawal, actor["token"])
        self.denied("ResultEvicted", "Artifact.ReadChunk", read, actor)
        self.verify()

    def test_cli_resumes_exact_authority_and_delegation_revocation_fences_a_sibling_stage(self):
        n = self.start()
        actor = {"principal_id": "local-owner", "token": n.owner}
        first = self.grant(actor, "artifact.upload")
        self.grant(actor, "artifact.upload")
        payload = b"\x87" * CHUNK + b"\x00\xffbound CLI proof"
        request, stage, stage_id = self.stage(actor, first, payload)
        self.put(actor, stage, 0, payload[:CHUNK])
        source = n.base / "bound.bin"
        source.write_bytes(payload)
        delegation_file = n.base / "delegation.json"
        delegation_file.write_text(json.dumps(first["delegation_ref"]))
        delegation_file.chmod(0o600)
        args = ("artifact-upload", "--delegation-ref", delegation_file, "--key", request["key"], source)
        saved = n.cli(n.witness / "owner.token", *args)
        self.assertEqual(saved["stage_id"], stage["stage_id"], saved)
        self.assertEqual(saved["reference"], self.current(actor, stage_id)["artifact_ref"])
        self.assertEqual(n.cli(n.witness / "owner.token", *args), saved)

        pending = b"proof that a revoked delegation can no longer publish"
        _, sibling, sibling_id = self.stage(actor, first, pending)
        self.put(actor, sibling, 0, pending)
        n.call("Delegation.Revoke", target(first["delegation_ref"]))
        grant = n.get(first["grant_ref"]["resource_id"])
        self.assertEqual(grant["value"]["lifecycle"], "Active")
        self.denied({"PermissionDenied", "PreconditionFailed"}, "Artifact.Publish", sibling_id, actor)
        self.reclaimed(actor, sibling_id)
        self.assertEqual(n.cli(n.witness / "owner.token", *args), saved)
        self.verify()

    @unittest.skipUnless(PRIOR_BINARIES, "set PSIOS_ARTIFACT_PRIOR_BIN_DIR to actual format 96 binaries")
    def test_legacy_partial_stage_survives_format_upgrade_and_regrant(self):
        n = self.start(PRIOR_BINARIES)
        actor = n.call("Agent.Register", {"name": "legacy-uploader"})
        first = self.grant(actor, "artifact.upload")
        payload = b"\x17" * CHUNK + b"legacy stage survives regrant\x00"
        request = upload_request(payload)
        staged = n.call("Artifact.Stage", request, actor["token"])
        stage_id = {"stage_id": staged["stage_id"]}
        offset = self.put(actor, staged, 0, payload[:CHUNK])
        n.call("Grant.Revoke", target(first["grant_ref"]))
        self.denied("PermissionDenied", "Artifact.Stage.Get", stage_id, actor)
        n.stop(signal.SIGKILL)
        n.daemon[0] = str(Path(BINARIES, "psiosd").absolute())
        before = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertEqual(before["database_format"], 96, before)
        self.assertTrue(before["migration_required"], before)
        n.start()
        self.denied("PermissionDenied", "Artifact.Stage.Get", stage_id, actor)
        self.grant(actor, "artifact.upload", "artifact.read")
        self.assertEqual(n.call("Artifact.Stage", request, actor["token"]), offset)
        self.assertEqual(self.current(actor, stage_id), offset)
        self.assertNotIn("delegation_ref", offset)
        self.assertEqual(self.storage(stage_id), ("Open", 1, 1))
        self.put(actor, staged, CHUNK, payload[CHUNK:])
        receipt = n.call("Artifact.Publish", stage_id, actor["token"])
        self.assertEqual(receipt["state"], "Published", receipt)
        trailer = n.call("Artifact.ReadChunk", {"reference": receipt["artifact_ref"], "offset": CHUNK,
                                                "length": len(payload) - CHUNK}, actor["token"])
        self.assertEqual(base64.b64decode(trailer["data_base64"]), payload[CHUNK:])
        after = self.verify()
        self.assertGreaterEqual(after["database_format"], 97, after)
        self.assertEqual(after["node_id"], before["node_id"])


if __name__ == "__main__":
    unittest.main()
