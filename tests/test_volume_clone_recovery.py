"""Recover a staged or published clone using actual filesystem and SQLite evidence."""

from contextlib import closing, contextmanager
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target


TOOL = "volume.clone"
INSPECT = "volume.inspect_publication"


class VolumeCloneRecovery(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.source = self.n.perform("volume.create", {"name": "clone-recovery-source"})
        self.source = self.n.perform("volume.write", {
            **target(self.source), "name": "record", "content": "source bytes\n",
        })["volume_ref"]
        self.database = self.n.state / "state.db"
        self.volumes = self.n.state / "volumes"
        self.source_file = self.volumes / self.source["resource_id"] / "data" / "record"
        self.grant = self.grant_for(TOOL, [self.source["resource_id"], "local"])

    def grant_for(self, tool, scope):
        reply = self.n.call("Grant.Issue", {"principal_id": "local-owner", "units": 12, "ttl_seconds": 180,
            "actions": ["task.submit", "task.read", tool], "tool_targets": scope})
        return reply["delegation_ref"]["resource_id"]

    def prepare(self):
        reference = self.n.base / "source-volume-ref.json"
        reference.write_text(json.dumps(self.source))
        return self.n.cli(self.n.witness / "owner.token", "volume-prepare-clone", "--name", "clone-recovered",
                          "--key", key(), "--delegation", self.grant, reference)

    def submit(self, request):
        task = self.n.call("Task.Submit", request)["task_id"]
        self.assertEqual(self.n.wait(task)["lifecycle"], "WaitingApproval")
        return task

    def request_error(self, code, method, params, token=None):
        with self.assertRaises(Failure) as rejected:
            self.n.call(method, params, token)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    @contextmanager
    def lose_catalog_receipt(self):
        with closing(sqlite3.connect(self.database, timeout=8)) as db, db:
            db.execute("""CREATE TRIGGER lose_clone_catalog BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.spec->>'cloned_from' IS NOT NULL
                BEGIN SELECT RAISE(ABORT, 'injected cloned Volume receipt loss'); END""")
        try:
            yield
        finally:
            with closing(sqlite3.connect(self.database, timeout=8)) as db, db:
                db.execute("DROP TRIGGER lose_clone_catalog")

    def checkpoint(self, task):
        uri = self.database.as_uri() + "?mode=ro"
        with closing(sqlite3.connect(uri, uri=True)) as db:
            row = db.execute("""SELECT q.operation, c.body FROM queue q JOIN current c
                ON c.id='restore-publication-'||q.operation AND c.kind='RestorePublicationCheckpoint'
                WHERE q.task=?""", (task,)).fetchone()
        self.assertIsNotNone(row)
        operation, body = row
        body = json.loads(body)
        self.assertNotIn("backup_ref", body)
        self.assertNotIn("manifest_digest", body)
        self.assertEqual(body["clone_source"]["volume_ref"], self.source)
        self.assertEqual(body["clone_source"]["pool_ref"]["resource_id"], "local")
        self.assertEqual(body["clone_source"]["name"], "clone-recovered")
        final = self.volumes / body["receipt"]["id"]
        self.assertEqual(final.name, f"volume-{operation}")
        stage = self.volumes / body["placement"]["stage_name"]
        return stage, final

    def inspect(self, op, delegation, placement):
        request = self.n.prepare(INSPECT, target(op), delegation=delegation)
        task = self.n.call("Task.Submit", request)["task_id"]
        result = self.n.wait(task)
        self.assertEqual(result["lifecycle"], "Succeeded", result)
        evidence, = result["result"]["artifact_refs"]
        report = self.n.call("Artifact.Read", {"reference": evidence})
        self.assertEqual((report["operation_ref"], report["publication"]), (op, placement))
        return evidence

    def test_catalog_receipt_loss_recovers_same_published_clone_without_recopying(self):
        n = self.n
        prepared = self.prepare()
        with self.lose_catalog_receipt():
            task = self.submit(prepared)
            n.approve(task)
            paused = n.wait(task)
            self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
            op, = paused["operation_refs"]
            stage, final = self.checkpoint(task)
            self.assertFalse(stage.exists())
            contents = final / "data" / "record"
            inode = contents.stat().st_ino
            self.assertNotEqual(inode, self.source_file.stat().st_ino)
            self.assertEqual(contents.read_bytes(), self.source_file.read_bytes())
            self.request_error("NotFound", "Resource.Get", {"id": final.name})
            n.stop(signal.SIGKILL)
        n.start()

        held = n.wait(n.submit("volume.write", {
            **target(self.source), "name": "must-not-change", "content": "fenced",
        }), autoapprove=True)
        self.assertEqual(held["conditions"][0]["type"], "Conflict", held)
        delegation = self.grant_for(INSPECT, [op["resource_id"]])
        first = self.inspect(op, delegation, "Published")
        stamp = contents.stat()
        payload = contents.read_bytes()
        contents.write_bytes(b"bad" + payload[3:])
        os.utime(contents, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.request_error("OutcomeUnknown", "Operation.Reconcile", {**target(op), "evidence_ref": first})
        self.assertEqual(n.wait(task), paused)
        contents.write_bytes(payload)
        os.utime(contents, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))

        recovered = n.call("Operation.Reconcile", {
            **target(op), "evidence_ref": self.inspect(op, delegation, "Published"),
        })
        self.assertEqual(recovered["outcome_knowledge"], "Known")
        result = n.wait(task)
        self.assertEqual(result["lifecycle"], "Succeeded")
        clone = n.call("Artifact.Read", {"reference": result["result"]["artifact_refs"][0]})
        self.assertEqual(clone["resource_id"], final.name)
        self.assertEqual(contents.stat().st_ino, inode)
        self.assertEqual(n.get(final.name)["resource"]["spec"]["cloned_from"], self.source)
        duplicate = n.call("Task.Submit", prepared)
        self.assertTrue(duplicate["deduplicated"])
        self.assertEqual(duplicate["task_id"], task)
        n.perform("volume.write", {**target(self.source), "name": "record", "content": "new source\n"})
        self.assertEqual(contents.read_bytes(), payload)
        n.perform("volume.activate", target(clone))
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_CLONE_RENAME_FAULT") == "1" and shutil.which("strace"),
                         "requires real strace renameat2 fault and process kill")
    def test_kill_before_atomic_publication_preserves_stage_and_allows_verified_disposal(self):
        n = self.n
        n.stop()
        settings = json.loads(n.config.read_text())
        n.config.write_text(json.dumps(dict(settings, queue_limit=1, recovery_queue_limit=1)))
        daemon, trace = n.daemon, n.base / "clone-rename.strace"
        n.daemon = ["strace", "-D", "-f", "-o", str(trace), "-e", "trace=renameat2",
                    "-e", "inject=renameat2:error=EINTR:signal=SIGKILL:when=1", "--", *daemon]
        try:
            n.start()
        finally:
            n.daemon = daemon
        self.assertTrue(os.path.samefile(f"/proc/{n.process.pid}/exe", daemon[0]))
        request = self.prepare()
        task = self.submit(request)
        try:
            n.approve(task)
        except (ConnectionError, struct.error):
            # The exact approval may commit before its response reaches the socket.
            pass
        self.assertEqual(n.process.wait(25), -signal.SIGKILL)

        stage, final = self.checkpoint(task)
        staged = stage.stat()
        self.assertFalse(final.exists())
        self.assertEqual((stage / "data" / "record").read_bytes(), self.source_file.read_bytes())
        entries = trace.read_text().splitlines()
        self.assertTrue(any("renameat2(" in line and f'"{stage.name}"' in line
                            and f'"{final.name}"' in line and "RENAME_NOREPLACE" in line for line in entries), entries)
        if prior := os.getenv("PSIOS_VOLUME_CLONE_PRIOR_BIN_DIR"):
            old = subprocess.run([Path(prior) / "psiosd", "--config", n.config, "serve"],
                                 capture_output=True, text=True, timeout=20)
            self.assertNotEqual(old.returncode, 0)
            self.assertIn("SchemaVersionUnsupported", old.stdout + old.stderr)
            self.assertEqual(stage.stat().st_ino, staged.st_ino)

        n.start()
        paused = n.wait(task)
        self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
        op, = paused["operation_refs"]
        self.assertEqual(stage.stat().st_ino, staged.st_ino)
        delegation = self.grant_for(INSPECT, [op["resource_id"]])
        proof = self.inspect(op, delegation, "NotApplied")
        fixed = n.call("Operation.Reconcile", {**target(op), "evidence_ref": proof})
        self.assertEqual(fixed["outcome_knowledge"], "Known")
        settled = n.wait(task)
        self.assertEqual((settled["lifecycle"], settled["conditions"][0]["type"]), ("Failed", "LeaseLost"))
        receipt = n.call("Artifact.Read", {"reference": settled["result"]["artifact_refs"][0]})
        self.assertEqual(receipt, {"source_volume_ref": self.source, "volume_id": final.name, "publication": "NotApplied"})
        deadline = time.monotonic() + 12
        while stage.exists() and time.monotonic() < deadline:
            time.sleep(0.06)
        self.assertFalse(stage.exists())
        self.assertFalse(final.exists())
        self.assertEqual(self.source_file.read_text(), "source bytes\n")
        self.assertTrue(n.call("Task.Submit", request)["deduplicated"])
        alternate = self.submit(self.prepare())
        n.approve(alternate)
        self.assertEqual(n.wait(alternate)["lifecycle"], "Succeeded")
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)
