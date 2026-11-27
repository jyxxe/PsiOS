"""Verify bounded creation keys, restart fencing, and durable receipt reclamation."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node
from test_artifact_ingress import chunk, upload_request
from test_artifact_install import grant


def uploader(node, name):
    actor = node.call("Agent.Register", {"name": name})
    authority = node.call("Node.Get", {})["reference"]["resource_id"]
    permitted = grant(node, actor["principal_id"], ["artifact.upload", "artifact.read"], [authority])
    return actor, permitted


def publish_upload(node, token, request, contents):
    stage = node.call("Artifact.Stage", request, token)
    node.call("Artifact.Stage.PutChunk", chunk(stage, 0, contents), token)
    return node.call("Artifact.Publish", {"stage_id": stage["stage_id"]}, token)


class PreparedUploadTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.agent, self.grant = uploader(self.node, "prepared-uploader")
        self.token = self.agent["token"]

    def declared(self, contents, *, bound=False):
        request = upload_request(contents, ttl_seconds=5)
        del request["key"]
        if bound:
            request["delegation_ref"] = self.grant["delegation_ref"]
        return request

    def prepare(self, contents, *, bound=False):
        requested = self.declared(contents, bound=bound)
        prepared = self.node.call("Artifact.Stage.Prepare", requested, self.token)
        return dict(requested, key=prepared["key"])

    def api(self, method, request):
        return self.node.call(method, request, self.token)

    def capacity(self):
        return self.api("Artifact.UploadCapacity", {})["principal"]

    def denied(self, expected, method, request, token=None):
        with self.assertRaises(Failure) as refused:
            self.node.call(method, request, token or self.token)
        self.assertEqual(refused.exception.code, expected, refused.exception)

    def reclaimed(self, identifier):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                self.api("Artifact.Stage.Get", {"stage_id": identifier})
            except Failure as error:
                self.assertEqual(error.code, "NotFound", error)
                return
            time.sleep(0.075)
        self.fail("expired prepared stage receipt was not reclaimed")

    def test_discarded_prepared_key_survives_restart_and_releases_capacity_without_replay(self):
        n = self.node
        n.stop()
        config = json.loads(n.config.read_text())
        n.config.write_text(json.dumps(dict(config, artifact_upload_history={"per_principal":1,"node":2})))
        n.start()
        body = b"private exact upload bytes"
        request = self.prepare(body, bound=True)
        self.assertEqual(self.capacity()["receipts"], 0)
        other, _ = uploader(n, "other-uploader")
        self.denied("PermissionDenied", "Artifact.Stage", request, other["token"])
        self.denied("PermissionDenied", "Artifact.Stage", dict(request, declared_media_type="text/plain"))
        self.denied("PermissionDenied", "Artifact.Stage", dict(request, key="psios-upload-v1-forged"))
        with ThreadPoolExecutor(max_workers=4) as callers:
            answers = list(callers.map(lambda _: self.api("Artifact.Stage", request), range(8)))
        stage = answers[0]
        self.assertEqual(answers, [stage] * len(answers))
        self.assertEqual(stage["delegation_ref"], self.grant["delegation_ref"])
        self.api("Artifact.Stage.PutChunk", chunk(stage, 0, body))
        params = {"stage_id": stage["stage_id"]}
        discarded = self.api("Artifact.Stage.Discard", params)
        self.assertEqual(discarded["state"], "Discarded")
        self.assertEqual(self.capacity()["remaining_receipts"], 0)
        self.assertEqual(self.capacity()["reserved_bytes"], 0)
        self.denied("QuotaExceeded", "Artifact.Stage", upload_request(b"next"))

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("""CREATE TRIGGER refuse_reclamation BEFORE DELETE ON artifact_uploads
                           WHEN OLD.state='Discarded' BEGIN SELECT raise(ABORT,'retain test receipt'); END""")
        n.start()
        self.assertEqual(self.api("Artifact.Stage", request), discarded)
        time.sleep(5.3)
        self.assertEqual(self.api("Artifact.Stage.Get", params), discarded)
        self.assertEqual(self.capacity()["remaining_receipts"], 0)
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            self.assertEqual(db.execute("SELECT count(*) FROM artifact_stage_windows").fetchone(), (1,))
            self.assertEqual(db.execute("SELECT count(*) FROM artifact_upload_authority").fetchone(), (1,))
            db.execute("DROP TRIGGER refuse_reclamation")
        n.start()
        self.reclaimed(stage["stage_id"])
        self.assertEqual(self.capacity()["remaining_receipts"], 1)
        self.denied("DeadlineExceeded", "Artifact.Stage", request)
        self.denied("PermissionDenied", "Artifact.Stage", dict(request, ttl_seconds=6))
        self.assertEqual(self.api("Artifact.Stage", upload_request(b"next"))["state"], "Open")
        audit = json.dumps(n.call("Audit.List", {}))
        self.assertIn("Artifact.Stage.Reclaim", audit)
        self.assertIn(stage["stage_id"], audit)

    def test_published_and_ordinary_receipts_keep_replay_while_unused_prepared_stage_expires(self):
        n = self.node
        payload = b"prepared publication"
        credentials = n.base / "uploader.token"
        credentials.touch(mode=0o600)
        credentials.write_text(self.token)
        metadata = n.base / "upload.json"
        fields = self.declared(payload)
        metadata.write_text(json.dumps(fields))
        prepared = n.cli(credentials, "artifact-prepare-upload", metadata)
        self.assertTrue(self.api("Artifact.UploadProfile", {})["prepared_stage_keys"])
        source = n.base / "source.bin"
        source.write_bytes(payload)
        command = ("artifact-upload", source, "--key", prepared["key"], "--ttl-seconds", "5",
                   "--declared-media-type", fields["declared_media_type"])
        published = n.cli(credentials, *command)
        ordinary = self.api("Artifact.Stage", upload_request(b"ordinary"))
        ordinary = self.api("Artifact.Stage.Discard", {"stage_id": ordinary["stage_id"]})
        expired_request = self.prepare(b"unused")
        expired = self.api("Artifact.Stage", expired_request)
        self.reclaimed(expired["stage_id"])
        self.denied("DeadlineExceeded", "Artifact.Stage", expired_request)
        self.assertEqual(n.cli(credentials, *command), published)
        self.assertEqual(self.api("Artifact.Stage.Get", {"stage_id": ordinary["stage_id"]}), ordinary)
        self.assertEqual(self.capacity()["receipts"], 2)
        n.stop()
        report = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 135)
        self.assertEqual(report["sqlite_integrity"], "Verified")

    def test_offline_verifier_rejects_a_shortened_persisted_window(self):
        n = self.node
        stage = self.api("Artifact.Stage", self.prepare(b"keep this original stage", bound=True))
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("UPDATE artifact_stage_windows SET deadline_boot_ms=deadline_boot_ms-1 WHERE stage_id=?",
                       (stage["stage_id"],))
        failed = subprocess.run([*n.daemon, "verify"], capture_output=True, text=True, timeout=15)
        self.assertNotEqual(failed.returncode, 0, failed)
        self.assertIn("prepared upload window disagrees with its stage receipt", failed.stderr)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("UPDATE artifact_stage_windows SET deadline_boot_ms=deadline_boot_ms+1 WHERE stage_id=?",
                       (stage["stage_id"],))
        subprocess.run([*n.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)

    def permit_release(self):
        authority = self.node.call("Node.Get", {})["reference"]["resource_id"]
        grant(self.node, self.agent["principal_id"], ["artifact.withdraw"], [authority])

    def test_withdrawn_prepared_upload_reclaims_one_slot_and_preserves_exact_release(self):
        n = self.node
        n.stop()
        config = json.loads(n.config.read_text())
        n.config.write_text(json.dumps(dict(config, artifact_upload_history={"per_principal": 1, "node": 2})))
        n.start()
        self.assertTrue(self.api("Artifact.UploadProfile", {})["reclaims_released_prepared_uploads"])
        payload = b"prepared then withdrawn"
        request = self.prepare(payload, bound=True)
        published = publish_upload(n, self.token, request, payload)
        stage = published["stage_id"]
        reference = published["artifact_ref"]
        release = {"reference": reference, "reason": "published content has been consumed"}
        self.denied("PermissionDenied", "Artifact.Withdraw", release)
        self.permit_release()

        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("""CREATE TRIGGER fail_reclamation BEFORE DELETE ON artifact_uploads
                           WHEN OLD.state='Withdrawn' BEGIN SELECT raise(ABORT,'reclamation cut'); END""")
        fault_log = n.base / "reclamation.log"
        with fault_log.open("wb") as output:
            n.start(stderr=output)
        time.sleep(5.2)
        self.assertTrue(self.api("Artifact.ReadChunk", {**reference, "offset": 0, "length": 1})["data_base64"])
        self.assertEqual(self.api("Artifact.Stage", request), published)
        self.denied("QuotaExceeded", "Artifact.Stage", upload_request(b"next item"))
        receipt = self.api("Artifact.Withdraw", release)
        self.assertEqual(self.capacity()["retained_bytes"], 0)
        self.assertEqual(self.api("Artifact.Stage.Get", {"stage_id": stage})["state"], "Withdrawn")
        self.assertEqual(self.capacity()["remaining_receipts"], 0)
        deadline = time.monotonic() + 10
        while b"reclamation cut" not in fault_log.read_bytes():
            self.assertLess(time.monotonic(), deadline, fault_log.read_text())
            time.sleep(0.05)

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            self.assertEqual(db.execute("SELECT count(*) FROM reclaimed_artifact_uploads").fetchone(), (0,))
            self.assertEqual(db.execute("SELECT reclaimable FROM artifact_stage_windows WHERE stage_id=?",
                                        (stage,)).fetchone(), (1,))
            self.assertEqual(db.execute("SELECT count(*) FROM artifact_upload_authority WHERE stage_id=?",
                                        (stage,)).fetchone(), (1,))
            db.execute("DROP TRIGGER fail_reclamation")
        n.start()
        self.reclaimed(stage)
        self.assertEqual(self.capacity()["remaining_receipts"], 1)
        self.denied("DeadlineExceeded", "Artifact.Stage", request)
        self.assertEqual(self.api("Artifact.Withdraw", release), receipt)
        self.denied("IdempotencyConflict", "Artifact.Withdraw", {**release, "reason": "other release"})
        self.denied("ResultEvicted", "Artifact.ReadChunk", {**reference, "offset": 0, "length": 1})
        self.assertEqual(self.api("Artifact.Stage", upload_request(b"next item"))["state"], "Open")
        audit = json.dumps(n.call("Audit.List", {}))
        self.assertIn("ReleasedPreparation", audit)
        self.assertIn(stage, audit)

        n.stop()
        report = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 143)
        self.assertEqual(report["sqlite_integrity"], "Verified")
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("UPDATE reclaimed_artifact_uploads SET release_digest=?", ("sha256:" + "0" * 64,))
        invalid = subprocess.run([*n.daemon, "verify"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("reclaimed upload disagrees with its immutable release receipt", invalid.stderr)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("UPDATE reclaimed_artifact_uploads SET release_digest=?",
                       (receipt["withdrawal_ref"]["digest"],))
        subprocess.run([*n.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    def test_abandoned_prepared_upload_preserves_damage_receipt_after_reclamation(self):
        n = self.node
        self.permit_release()
        payload = b"prepared damaged bytes"
        request = self.prepare(payload)
        published = publish_upload(n, self.token, request, payload)
        reference = published["artifact_ref"]
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DELETE FROM artifact_chunks WHERE artifact_id=?", (reference["resource_id"],))
        n.start()
        release = {"reference": reference, "reason": "remaining backup copy is unavailable"}
        self.denied("RecoveryRequired", "Artifact.Withdraw", release)
        receipt = self.api("Artifact.AbandonUpload", release)
        self.reclaimed(published["stage_id"])
        self.assertEqual(self.capacity()["receipts"], 0)
        self.assertEqual(self.api("Artifact.AbandonUpload", release), receipt)
        self.denied("IdempotencyConflict", "Artifact.Withdraw", release)
        self.denied("DeadlineExceeded", "Artifact.Stage", request)
        n.stop()
        subprocess.run([*n.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_UPLOAD_RELEASE_PREVIOUS_BIN_DIR"), "requires format-142 Core")
    def test_format_142_published_receipt_without_window_remains_replayable(self):
        prior = Node(Path(os.environ["PSIOS_UPLOAD_RELEASE_PREVIOUS_BIN_DIR"]))
        self.addCleanup(prior.close)
        actor, credential = uploader(prior, "previous-prepared-publisher")
        token = actor["token"]
        authority = prior.call("Node.Get", {})["reference"]["resource_id"]
        grant(prior, actor["principal_id"], ["artifact.withdraw"], [authority])
        payload = b"publication without a recorded preparation window"
        declared = self.declared(payload)
        declared["delegation_ref"] = credential["delegation_ref"]
        prepared = prior.call("Artifact.Stage.Prepare", declared, token)
        request = {**declared, "key": prepared["key"]}
        published = publish_upload(prior, token, request, payload)
        stage = {"stage_id": published["stage_id"]}
        release = {"reference": published["artifact_ref"], "reason": "retained old publication"}
        receipt = prior.call("Artifact.Withdraw", release, token)
        original = prior.call("Artifact.Stage.Get", stage, token)
        prior.stop(signal.SIGKILL)
        old = prior.daemon[:]
        prior.daemon[0] = str(self.node.binaries / "psiosd")
        pending = json.loads(subprocess.check_output([*prior.daemon, "verify"], timeout=20))
        self.assertEqual((pending["database_format"], pending["migration_required"]), (142, True))
        prior.start()
        time.sleep(5.2)
        self.assertEqual(prior.call("Artifact.Stage", request, token), original)
        self.assertEqual(prior.call("Artifact.Withdraw", release, token), receipt)
        self.assertEqual(prior.call("Artifact.UploadCapacity", {}, token)["principal"]["receipts"], 1)
        prior.stop()
        rejected = subprocess.run([*old, "serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("SchemaVersionUnsupported", rejected.stderr)
        subprocess.run([*prior.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_UPLOAD_WINDOW_PREVIOUS_BIN_DIR"), "requires the format-134 Core")
    def test_upgrade_keeps_a_legacy_key_and_fences_the_previous_writer(self):
        prior = Node(Path(os.environ["PSIOS_UPLOAD_WINDOW_PREVIOUS_BIN_DIR"]))
        self.addCleanup(prior.close)
        actor, _ = uploader(prior, "legacy-uploader")
        token = actor["token"]
        request = upload_request(b"existing historical stage", key="psios-upload-v1-legacy-request")
        staged = prior.call("Artifact.Stage", request, token)
        discarded = prior.call("Artifact.Stage.Discard", {"stage_id": staged["stage_id"]}, token)
        prior.stop(signal.SIGKILL)
        original = prior.daemon[:]
        old = json.loads(subprocess.check_output([*original, "verify"], timeout=20))
        self.assertEqual((old["database_format"], old["migration_required"]), (134, False))

        prior.binaries = self.node.binaries
        prior.daemon[0] = str(prior.binaries / "psiosd")
        prior.start()
        self.assertEqual(prior.call("Artifact.Stage", request, token), discarded)
        current = prior.call("Artifact.UploadProfile", {}, token)
        self.assertTrue(current["prepared_stage_keys"])
        prior.stop()
        upgraded = json.loads(subprocess.check_output([*prior.daemon, "verify"], timeout=20))
        self.assertEqual(upgraded["node_id"], old["node_id"])
        self.assertGreaterEqual(upgraded["database_format"], 135)
        self.assertFalse(upgraded["migration_required"])
        denied = subprocess.run([*original, "serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn("SchemaVersionUnsupported", denied.stderr)
