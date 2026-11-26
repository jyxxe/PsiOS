"""Recover an ordinary volume.create at actual directory publication boundaries."""

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

from support import Failure, Node, target


CREATE = "volume.create"
INSPECT = "volume.inspect_publication"
PRIOR_BIN = os.getenv("PSIOS_VOLUME_CREATE_PRIOR_BIN_DIR")


def identity(path):
    metadata = path.stat()
    return metadata.st_dev, metadata.st_ino


class VolumeCreateCustody(unittest.TestCase):
    def setUp(self):
        self.current_bin = Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).absolute()

    def open_node(self, binaries=None):
        node = Node(binaries or self.current_bin)
        self.addCleanup(node.close)
        return node

    def deny(self, node, code, method, params):
        with self.assertRaises(Failure) as rejected:
            node.call(method, params)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    @contextmanager
    def lose_catalogue(self, node, name):
        database = node.state / "state.db"
        with closing(sqlite3.connect(database, timeout=10)) as db, db:
            db.execute(f"""CREATE TRIGGER volume_create_fixture_no_catalogue BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.spec->>'name'='{name}'
                BEGIN SELECT RAISE(ABORT,'fixture withheld the created Volume catalogue'); END""")
        try:
            yield
        finally:
            with closing(sqlite3.connect(database, timeout=10)) as db, db:
                db.execute("DROP TRIGGER volume_create_fixture_no_catalogue")

    def unknown(self, node, task_id):
        task = node.wait(task_id, autoapprove=True)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Paused", "Unknown"), task)
        operation, = task["operation_refs"]
        return task, operation

    def checkpoint(self, node, operation, name, pool):
        operation_id = operation["resource_id"]
        checkpoint = node.get(f"restore-publication-{operation_id}")
        self.assertEqual(checkpoint["resource"]["kind"], "RestorePublicationCheckpoint", checkpoint)
        self.assertEqual(checkpoint["resource"]["spec"]["operation_id"], operation_id)
        value = checkpoint["value"]
        self.assertEqual(value["create_source"], {"pool_ref": pool, "name": name}, value)
        self.assertNotIn("backup_ref", value)
        self.assertNotIn("clone_source", value)
        receipt = value["receipt"]
        self.assertEqual((receipt["id"], receipt["incarnation"], receipt["restore_proof"]),
                         (f"volume-{operation_id}", f"data-{operation_id}", None), receipt)
        parent = node.state / "volumes"
        placement = value["placement"]
        self.assertEqual(placement["parent"], list(map(str, identity(parent))), placement)
        return checkpoint, parent / placement["stage_name"], parent / receipt["id"]

    def exact_directory(self, root, checkpoint):
        value = checkpoint["value"]
        receipt = value["receipt"]
        self.assertEqual(list(map(str, identity(root))), value["placement"]["directory"])
        self.assertEqual(list(map(str, identity(root / "data"))), [receipt["device"], receipt["inode"]])
        self.assertEqual({entry.name for entry in root.iterdir()}, {"identity", "data"})
        self.assertEqual(json.loads((root / "identity").read_bytes()), [receipt["id"], receipt["incarnation"]])
        self.assertEqual(list((root / "data").iterdir()), [])

    def inspection_grant(self, node, operation):
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 16, "ttl_seconds": 180,
            "actions": ["task.submit", "task.read", INSPECT], "tool_targets": [operation["resource_id"]]})
        return grant["delegation_ref"]["resource_id"]

    def inspect(self, node, operation, delegation, *, phase=None, checkpoint=None, error=None):
        request = node.prepare(INSPECT, target(operation), delegation=delegation)
        try:
            accepted = node.call("Task.Submit", request)
        except Failure as rejected:
            if error is None:
                raise
            self.assertEqual(rejected.code, error, rejected)
            return None
        task = node.wait(accepted["task_id"])
        if error is not None:
            self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]), ("Failed", error), task)
            self.assertEqual(task["result"]["artifact_refs"], [], task)
            return None
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        evidence, = task["result"]["artifact_refs"]
        report = node.call("Artifact.Read", {"reference": evidence})
        self.assertEqual((report["operation_ref"], report["checkpoint_ref"], report["publication"]),
                         (operation, checkpoint["reference"], phase), report)
        self.assertRegex(report["metadata_digest"], r"^sha256:[0-9a-f]{64}$")
        self.assertIsNone(report["restore_proof"], report)
        self.assertTrue(report["observed_at"], report)
        return evidence

    def settled_ref(self, node, task_id):
        task = node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        return node.call("Artifact.Read", {"reference": artifact})

    def verify(self, command):
        report = json.loads(subprocess.check_output(command + ["verify"], timeout=20))
        self.assertEqual(report["sqlite_integrity"], "Verified", report)
        return report

    def start_logged(self, node, log):
        with log.open("ab") as output:
            node.process = subprocess.Popen(node.daemon + ["serve"], stderr=output)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and node.process.poll() is None:
            try:
                node.call("Principal.WhoAmI", {})
                return
            except (ConnectionError, FileNotFoundError):
                time.sleep(0.02)
        self.fail(log.read_text())

    def test_lost_catalogue_publishes_only_the_exact_original_empty_directory(self):
        node = self.open_node()
        name = "created-volume-catalogue-fault"
        pool = node.get("local")["reference"]
        request = node.prepare(CREATE, {"name": name})
        with self.lose_catalogue(node, name):
            accepted = node.call("Task.Submit", request)
            task_id = accepted["task_id"]
            paused, operation = self.unknown(node, task_id)
            checkpoint, stage, final = self.checkpoint(node, operation, name, pool)
            self.exact_directory(final, checkpoint)
            inode = identity(final / "data")
            self.assertFalse(stage.exists())
            self.deny(node, "NotFound", "Resource.Get", {"id": final.name})
            node.stop(signal.SIGKILL)
        self.assertFalse(self.verify(node.daemon)["migration_required"])
        node.start()
        self.assertEqual(node.wait(task_id), paused)
        self.assertEqual(node.get(checkpoint["reference"]["resource_id"]), checkpoint)
        self.exact_directory(final, checkpoint)
        node.call("StoragePool.Configure", {**target(pool), "allocation_enabled": False})
        self.assertNotEqual(node.get("local")["reference"], pool)
        self.assertFalse(node.get("local")["value"]["allocation_enabled"])
        denied = node.wait(node.submit(CREATE, {"name": "new-allocation-while-disabled"}), autoapprove=True)
        self.assertEqual((denied["lifecycle"], denied["conditions"][0]["type"]),
                         ("Failed", "PreconditionFailed"), denied)
        grant = self.inspection_grant(node, operation)
        evidence = self.inspect(node, operation, grant, phase="Published", checkpoint=checkpoint)

        def reject_changed_placement():
            inputs = {**target(operation), "evidence_ref": evidence}
            self.deny(node, "OutcomeUnknown", "Operation.Reconcile", inputs)
            self.inspect(node, operation, grant, error="PreconditionFailed")
            self.deny(node, "NotFound", "Resource.Get", {"id": final.name})
            self.assertEqual(node.wait(task_id), paused)

        kept = final.with_name("saved-fixture-volume")
        final.rename(kept)
        try:
            reject_changed_placement()  # Neither the checkpointed stage nor its final name is present.
            final.mkdir(mode=0o700)
            try:
                (final / "data").mkdir(mode=0o700)
                (final / "identity").write_bytes((kept / "identity").read_bytes())
                self.assertNotEqual(identity(final), identity(kept))
                reject_changed_placement()  # Plausible empty replacement with the same marker.
                self.assertTrue(final.is_dir())
            finally:
                shutil.rmtree(final)
        finally:
            kept.rename(final)

        foreign = final / "data" / "unexpected"
        data_stamp = foreign.parent.stat()
        foreign.write_bytes(b"retain this unknown file")
        try:
            reject_changed_placement()
            self.assertEqual(foreign.read_bytes(), b"retain this unknown file")
        finally:
            foreign.unlink()
            os.utime(foreign.parent, ns=(data_stamp.st_atime_ns, data_stamp.st_mtime_ns))

        exact = self.inspect(node, operation, grant, phase="Published", checkpoint=checkpoint)
        settled = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": exact})
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"), settled)
        volume = self.settled_ref(node, task_id)
        self.assertEqual(volume, node.get(final.name)["reference"])
        self.assertEqual(node.get(final.name)["resource"]["spec"]["name"], name)
        self.assertEqual(identity(final / "data"), inode)
        self.exact_directory(final, checkpoint)
        self.assertFalse(stage.exists())
        self.assertEqual(node.call("Task.Submit", request), {**accepted, "deduplicated": True})
        self.assertEqual(self.settled_ref(node, task_id), volume)
        node.call("StoragePool.Configure", {**target(node.get("local")["reference"]), "allocation_enabled": True})
        node.stop()
        self.assertFalse(self.verify(node.daemon)["migration_required"])

        # Keep the valid manifest and chunks but remove the original Task's
        # receipt link. Offline verification must authenticate the settlement.
        with closing(sqlite3.connect(node.state / "state.db")) as db, db:
            revision, body = db.execute("SELECT rev,body FROM current WHERE id=?", (task_id,)).fetchone()
            db.execute("UPDATE versions SET body=json_set(body,'$.result.artifact_refs',json('[]')) WHERE id=? AND rev=?",
                       (task_id, revision))
        try:
            checked = subprocess.run(node.daemon + ["verify"], capture_output=True, text=True, timeout=20)
            self.assertNotEqual(checked.returncode, 0, checked)
            self.assertIn("volume creation settlement does not retain its exact catalog receipt", checked.stderr)
        finally:
            with closing(sqlite3.connect(node.state / "state.db")) as db, db:
                db.execute("UPDATE versions SET body=? WHERE id=? AND rev=?", (body, task_id, revision))
        self.assertFalse(self.verify(node.daemon)["migration_required"])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_CREATE_RENAME_FAULT") == "1" and shutil.which("strace"),
                         "requires an actual renameat2 fault and process kill")
    def test_pre_rename_crash_discards_only_the_verified_original_stage(self):
        node = self.open_node()
        name = "not-applied-volume-create"
        pool = node.get("local")["reference"]
        node_ref = node.call("Node.Get", {})["reference"]
        node.call("Node.Configure", {"expected_resource_version": node_ref["version"], "scheduling": "Cordoned"})
        node.stop()
        daemon, trace = node.daemon, node.base / "volume-create-rename.strace"
        node.daemon = ["strace", "-D", "-f", "-o", str(trace), "-e", "trace=renameat2",
                       "-e", "inject=renameat2:error=EINTR:signal=SIGKILL:when=1", "--", *daemon]
        try:
            node.start()
        finally:
            node.daemon = daemon
        self.assertTrue(os.path.samefile(f"/proc/{node.process.pid}/exe", daemon[0]))
        request = node.prepare(CREATE, {"name": name})
        accepted = node.call("Task.Submit", request)
        task_id = accepted["task_id"]
        self.assertEqual(node.call("Task.Get", {"id": task_id})["lifecycle"], "Queued")
        try:
            node_ref = node.call("Node.Get", {})["reference"]
            node.call("Node.Configure", {"expected_resource_version": node_ref["version"], "scheduling": "Enabled"})
            task = node.wait(task_id)
            if task["lifecycle"] == "WaitingApproval":
                node.approve(task_id)
        except (ConnectionError, FileNotFoundError, struct.error):
            # The expected process kill can interrupt a concurrent API response.
            pass
        self.assertEqual(node.process.wait(25), -signal.SIGKILL)
        self.assertFalse(self.verify(daemon)["migration_required"])
        log = node.base / "volume-create-reclaim.log"
        self.start_logged(node, log)
        paused, operation = self.unknown(node, task_id)
        self.assertEqual(paused["task_id"], task_id)
        checkpoint, stage, final = self.checkpoint(node, operation, name, pool)
        self.exact_directory(stage, checkpoint)
        self.assertFalse(final.exists())
        self.assertTrue(any("renameat2(" in line and f'"{stage.name}"' in line and f'"{final.name}"' in line
                            and "RENAME_NOREPLACE" in line and "INJECTED" in line
                            for line in trace.read_text().splitlines()), trace.read_text())

        other = stage.with_name("operator-owned-directory")
        other.mkdir(mode=0o700)
        marker = other / "keep"
        marker.write_bytes(b"unrelated bytes")
        grant = self.inspection_grant(node, operation)
        proof = self.inspect(node, operation, grant, phase="NotApplied", checkpoint=checkpoint)
        disposal = stage.with_name(f".restore-discard-{operation['resource_id']}")
        disposal.mkdir(mode=0o700)
        sentinel = disposal / "not-owned-by-the-operation"
        sentinel.write_bytes(b"retirement must not replace this directory")
        settled = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": proof})
        self.assertEqual(settled["outcome_knowledge"], "Known", settled)
        failed = node.wait(task_id)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                         ("Failed", "Known", "LeaseLost"), failed)
        self.exact_directory(stage, checkpoint)
        self.assertEqual(sentinel.read_bytes(), b"retirement must not replace this directory")
        unexpected = stage / "data" / "unexpected-after-settlement"
        unexpected.write_bytes(b"retain these bytes")
        sentinel.unlink()
        disposal.rmdir()
        deadline = time.monotonic() + 10
        while "restore stage reclamation: PreconditionFailed" not in log.read_text() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertIn("restore stage reclamation: PreconditionFailed", log.read_text())
        self.assertEqual(unexpected.read_bytes(), b"retain these bytes")
        self.assertFalse(disposal.exists())
        unexpected.unlink()
        deadline = time.monotonic() + 12
        while (stage.exists() or disposal.exists()) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(stage.exists())
        self.assertFalse(disposal.exists())
        self.assertFalse(final.exists())
        self.assertEqual(marker.read_bytes(), b"unrelated bytes")
        self.deny(node, "NotFound", "Resource.Get", {"id": final.name})
        self.assertEqual(node.call("Task.Submit", request), {**accepted, "deduplicated": True})
        self.assertEqual(node.wait(task_id), failed)
        node.stop()
        self.assertFalse(self.verify(node.daemon)["migration_required"])

    @unittest.skipUnless(PRIOR_BIN, "requires genuine format 117 binaries")
    def test_legacy_unknown_without_checkpoint_stays_unknown_after_migration(self):
        node = self.open_node(PRIOR_BIN)
        old_daemon = node.daemon[:]
        name = "legacy-unknown-volume-create"
        request = node.prepare(CREATE, {"name": name})
        with self.lose_catalogue(node, name):
            accepted = node.call("Task.Submit", request)
            task_id = accepted["task_id"]
            paused, operation = self.unknown(node, task_id)
            final = node.state / "volumes" / f"volume-{operation['resource_id']}"
            inode = identity(final / "data")
            self.deny(node, "NotFound", "Resource.Get", {"id": final.name})
            self.deny(node, "NotFound", "Resource.Get", {"id": f"restore-publication-{operation['resource_id']}"})
            node.stop(signal.SIGKILL)
        previous = self.verify(old_daemon)
        self.assertEqual(previous["database_format"], 117, previous)
        node.daemon[0] = str(self.current_bin / "psiosd")
        pending = self.verify(node.daemon)
        self.assertTrue(pending["migration_required"], pending)
        self.assertEqual(pending["node_id"], previous["node_id"])
        node.start()
        self.assertEqual(node.wait(task_id), paused)
        grant = self.inspection_grant(node, operation)
        self.inspect(node, operation, grant, error="RecoveryRequired")
        self.assertEqual(node.call("Task.Submit", request), {**accepted, "deduplicated": True})
        self.assertEqual(node.wait(task_id), paused)
        self.assertEqual(identity(final / "data"), inode)
        self.deny(node, "NotFound", "Resource.Get", {"id": final.name})
        fresh = node.perform(CREATE, {"name": "ordinary-create-after-upgrade"})
        self.assertNotEqual(fresh["resource_id"], final.name)
        self.assertEqual(identity(final / "data"), inode)
        node.stop()
        current = self.verify(node.daemon)
        self.assertGreaterEqual(current["database_format"], 118, current)
        self.assertFalse(current["migration_required"], current)
        old_writer = subprocess.run(old_daemon + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(old_writer.returncode, 0, old_writer)
        self.assertIn("SchemaVersionUnsupported", old_writer.stderr)
