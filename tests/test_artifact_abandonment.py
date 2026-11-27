"""Release proven lost SQLite payloads with original custody and quota intact."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target
from test_artifact_ingress import CHUNK, MAX, upload_request
from test_artifact_install import TOOL, grant, prepare, publish_install


class ArtifactAbandonmentTests(unittest.TestCase):
    def setUp(self):
        self.n = Node()
        self.addCleanup(self.n.close)
        self.node_id = self.n.call("Node.Get", {})["reference"]["resource_id"]

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as refused:
            self.n.call(method, params, token)
        self.assertEqual(refused.exception.code, code, refused.exception)

    def change_fixture(self, *statements):
        self.n.stop()
        with closing(sqlite3.connect(self.n.state / "state.db")) as db, db:
            for sql, args in statements:
                db.execute(sql, args)
        self.n.start()

    def lose_chunk(self, artifact, index):
        self.change_fixture(("DELETE FROM artifact_chunks WHERE artifact_id=? AND chunk_index=?",
                             (artifact["resource_id"], index)))

    def upload_actor(self, name, actions):
        actor = self.n.call("Agent.Register", {"name": name})
        grant(self.n, actor["principal_id"], actions, [self.node_id])
        return actor

    def refuse_receipt(self, kind):
        self.assertIn(kind, ("TaskResultAbandonment", "ArtifactAbandonment"))
        return (f"""CREATE TRIGGER refuse_abandonment BEFORE INSERT ON versions
                    WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT, 'test receipt failure'); END""", ())

    def verify_after_restart(self):
        self.n.stop(signal.SIGKILL)
        command = self.n.daemon + ["verify", "--scratch-dir", str(self.n.base)]
        report = json.loads(subprocess.check_output(command, timeout=35))
        self.assertGreaterEqual(report["database_format"], 126)
        self.assertEqual(report["witness"], "Confirmed")
        self.n.start()

    def test_task_loss_releases_quota_once_after_current_protection_and_repair_checks(self):
        n = self.n
        n.stop()
        config = json.loads(n.config.read_text())
        config["read_result_bytes_per_principal"] = 4 * 1024 * 1024
        n.config.write_text(json.dumps(config))
        n.start()
        volume = n.perform("volume.create", {"name": "abandoned-task-data"})
        data = n.state / "volumes" / volume["resource_id"] / "data"
        # Escaping NUL in the result makes two chunks without exceeding the text read limit.
        volume = n.perform("volume.write", dict(target(volume), name="record",
                           content="\0" * (24 * 1024) + "尾"))["volume_ref"]
        task = n.wait(n.submit("volume.read", dict(target(volume), name="record")))
        self.assertEqual((task["lifecycle"], task["result"]["availability"]), ("Succeeded", "Available"), task)
        task_id = task["task_id"]
        ref, = task["result"]["artifact_refs"]
        where = {"reference": ref}
        original = n.call("Artifact.Get", where)["resource"]
        charge = original["spec"]["size_bytes"]
        needed = n.call("Artifact.ReadChunk", dict(where, offset=CHUNK, length=CHUNK))["data_base64"]
        run = {"id": task["current_run_id"]}
        operation = {"id": task["operation_refs"][0]["resource_id"]}
        execution = (n.call("Run.Get", run), n.call("Operation.Get", operation))

        def usage():
            return n.call("Task.Admission", {})["read_results"]["retained_bytes"]

        current = target(n.get(task_id)["reference"])
        self.denied("PreconditionFailed", "Task.AbandonResults", dict(current, reason="still healthy"))
        dependent_request = n.call("Task.Prepare", {
            "tool": "volume.write", "key": key(), "delegation_id": "owner-delegation",
            "input": dict(target(volume), name="dependent", content="must remain unwritten"),
            "requires": [{"task_id": task_id, "digest": ref["digest"]}],
        })
        dependent = n.call("Task.Submit", dependent_request)["task_id"]
        self.assertEqual(n.wait(dependent)["lifecycle"], "WaitingApproval")

        self.lose_chunk(ref, 1)
        unavailable = n.call("Task.VerifyResults", current)
        self.assertEqual(unavailable["result"]["availability"], "Unavailable")
        request = dict(id=task_id, expected_resource_version=unavailable["task_revision"], reason="lost task payload")
        self.denied("PreconditionFailed", "Task.EvictResults", request)
        self.denied("Conflict", "Task.AbandonResults", request)
        outsider, _ = n.agent(["task.submit", "task.read"])
        self.denied("PermissionDenied", "Task.AbandonResults", request, outsider["token"])
        n.call("Task.Cancel", target(n.get(dependent)["reference"]))
        self.assertFalse((data / "dependent").exists())

        n.call("Artifact.RepairChunk", dict(where, offset=CHUNK, data_base64=needed))
        self.denied("PreconditionFailed", "Task.AbandonResults", request)
        self.lose_chunk(ref, 1)
        self.change_fixture(self.refuse_receipt("TaskResultAbandonment"))
        self.denied("StorageUnavailable", "Task.AbandonResults", request)
        self.assertEqual(usage(), charge)
        self.assertEqual(n.call("Task.Get", {"id": task_id}), unavailable)
        self.assertIsNone(n.call("Artifact.Get", where)["retention"])
        self.assertTrue(n.call("Artifact.ReadChunk", dict(where, offset=0, length=1))["data_base64"])
        with self.assertRaises(Failure) as blocked:
            n.submit("system.status", {})
        self.assertEqual(blocked.exception.code, "QuotaExceeded")
        self.change_fixture(("DROP TRIGGER refuse_abandonment", ()))

        with ThreadPoolExecutor(max_workers=2) as calls:
            first, duplicate = list(calls.map(lambda _: n.call("Task.AbandonResults", request), range(2)))
        self.assertEqual(first, duplicate)
        evidence = first["abandonment"]
        self.assertEqual(evidence["logical_bytes_released"], charge)
        self.assertEqual(evidence["content_bytes_removed"], str(CHUNK))
        self.assertEqual(evidence["damage"], [{"artifact_ref": ref, "condition": "MissingChunks",
                          "expected_chunks": 2, "surviving_chunks": 1, "surviving_bytes": str(CHUNK)}])
        self.assertEqual(n.call("Artifact.Get", where)["resource"], original)
        self.assertEqual(n.call("Artifact.Get", where)["retention"], first)
        self.assertEqual((n.call("Run.Get", run), n.call("Operation.Get", operation)), execution)
        settled = n.call("Task.Get", {"id": task_id})
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"], settled["result"]),
                         ("Succeeded", "Known", {"availability": "Evicted", "artifact_refs": [ref]}))
        self.assertEqual(usage(), "0")
        self.assertEqual(n.wait(n.submit("system.status", {}))["lifecycle"], "Succeeded")
        self.denied("ResultEvicted", "Artifact.Read", where)
        self.denied("Conflict", "Task.EvictResults", request)
        self.verify_after_restart()
        cli = [n.binaries / "psiosctl", "--config", n.config, "task-abandon-results", task_id,
               "--revision", request["expected_resource_version"], "--reason", request["reason"]]
        self.assertEqual(json.loads(subprocess.check_output(cli, timeout=15)), first)

    def test_upload_loss_frees_real_owner_quota_and_keeps_original_manifest(self):
        n = self.n
        publisher = self.upload_actor("upload-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"])
        token = publisher["token"]
        token_file = n.base / "publisher.token"
        token_file.write_text(token)
        token_file.chmod(0o600)
        fixture = n.base / "upload.bin"
        published = []
        for index in range(4):
            fixture.write_bytes(bytes([index + 1]) * MAX)
            published.append(n.cli(token_file, "artifact-upload", fixture, "--key", key()))
        ref = published[0]["reference"]
        where = {"reference": ref}
        original = n.call("Artifact.Get", where, token)["resource"]
        request = dict(where, reason="lost upload chunk")
        small = upload_request(b"replacement")
        self.denied("PreconditionFailed", "Artifact.AbandonUpload", request, token)
        self.denied("QuotaExceeded", "Artifact.Stage", small, token)
        self.lose_chunk(ref, MAX // CHUNK - 1)
        self.denied("RecoveryRequired", "Artifact.Withdraw", request, token)
        outsider = self.upload_actor("upload-outsider", ["artifact.withdraw"])
        self.denied("PermissionDenied", "Artifact.AbandonUpload", request, outsider["token"])
        self.change_fixture(self.refuse_receipt("ArtifactAbandonment"))
        self.denied("StorageUnavailable", "Artifact.AbandonUpload", request, token)
        self.denied("QuotaExceeded", "Artifact.Stage", small, token)
        self.assertTrue(n.call("Artifact.ReadChunk", dict(where, offset=0, length=1), token)["data_base64"])
        self.change_fixture(("DROP TRIGGER refuse_abandonment", ()))

        cli = [n.binaries / "psiosctl", "--config", n.config, "--token-file", token_file,
               "artifact-abandon-upload", "--reason", request["reason"]]
        first = json.loads(subprocess.check_output(cli, input=json.dumps(ref).encode(), timeout=25))
        self.assertEqual(first["artifact_ref"], ref)
        current = n.call("Artifact.Get", where, token)
        self.assertEqual((current["availability"], current["resource"]), ("Evicted", original))
        receipt = current["retention"]["abandonment"]
        self.assertEqual(current["retention"]["reference"], first["abandonment_ref"])
        self.assertEqual(receipt["logical_bytes_released"], str(MAX))
        self.assertEqual(receipt["content_bytes_removed"], str(MAX - CHUNK))
        self.assertEqual(receipt["damage"]["condition"], "MissingChunks")
        self.assertEqual(n.call("Artifact.Stage.Get", {"stage_id": published[0]["stage_id"]}, token)["artifact_ref"], ref)
        new_stage = n.call("Artifact.Stage", small, token)
        n.call("Artifact.Stage.Discard", {"stage_id": new_stage["stage_id"]}, token)
        self.denied("ResultEvicted", "Artifact.ReadChunk", dict(where, offset=0, length=1), token)
        self.denied("IdempotencyConflict", "Artifact.Withdraw", request, token)
        self.verify_after_restart()
        self.assertEqual(n.call("Artifact.AbandonUpload", request, token), first)

    def test_damaged_upload_stays_pinned_by_a_waiting_installation(self):
        n = self.n
        publisher = self.upload_actor("install-publisher", ["artifact.upload", "artifact.read", "artifact.withdraw"])
        token = publisher["token"]
        spec = {"name": "retained-upload", "image_id": "sha256:" + "a" * 64,
                "argv": ["/run"], "release_version": "1.0.0", "license_expression": "0BSD", "cpu_millicores": 100,
                "memory_max_bytes": 32 * 1024 * 1024, "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024}
        ref = publish_install(n, spec, token)
        allowed = grant(n, publisher["principal_id"], ["task.submit", "task.read", TOOL],
                        ["profile-local", ref["resource_id"]])
        task_id = n.call("Task.Submit", prepare(n, ref, allowed, token)["request"], token)["task_id"]
        self.assertEqual(n.wait(task_id)["lifecycle"], "WaitingApproval")
        self.lose_chunk(ref, 0)
        request = {"reference": ref, "reason": "installation source lost"}
        self.denied("Conflict", "Artifact.AbandonUpload", request, token)
        n.call("Task.Cancel", target(n.get(task_id)["reference"]), token)
        self.assertTrue(n.call("Artifact.AbandonUpload", request, token)["abandonment_ref"])
        self.assertEqual(n.call("Resource.List", {"kind": "Application"})["items"], [])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_ARTIFACT_PREVIOUS_BIN_DIR"), "requires the format-125 Core")
    def test_format_125_receipt_survives_and_old_writer_is_fenced(self):
        previous = Path(os.environ["PSIOS_TEST_ARTIFACT_PREVIOUS_BIN_DIR"]).resolve()
        old = Node(previous)
        self.addCleanup(old.close)
        task = old.wait(old.submit("system.status", {}))
        request = dict(target(old.get(task["task_id"])["reference"]), reason="original successful eviction")
        expected = old.call("Task.EvictResults", request)
        artifact, = task["result"]["artifact_refs"]
        old.stop(signal.SIGKILL)
        old.daemon[0] = str(self.n.binaries / "psiosd")
        command = old.daemon + ["verify", "--scratch-dir", str(old.base)]
        before = json.loads(subprocess.check_output(command, timeout=20))
        self.assertEqual((before["database_format"], before["migration_required"]), (125, True))
        old.start()
        self.assertEqual(old.call("Task.EvictResults", request), expected)
        self.assertEqual(old.call("Artifact.Get", {"reference": artifact})["retention"], expected)
        old.stop()
        denied = subprocess.run([previous / "psiosd", "--config", old.config, "check"],
                                capture_output=True, text=True, timeout=15)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn("SchemaVersionUnsupported", denied.stderr)
