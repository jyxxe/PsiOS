"""Exercise bounded reads and interrupted publication using the real authority."""
import base64
from contextlib import closing
import hashlib
import json
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, target


CHUNK = 128 * 1024


class ArtifactStorageTests(unittest.TestCase):
    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)
        volume = self.n.perform("volume.create", {"name": "artifact-storage"})
        # JSON escapes these UTF-8 bytes, producing a result longer than two
        # chunks while staying within the ordinary volume text tool's limits.
        self.content = "\0" * (48 * 1024) + "尾"
        volume = self.n.perform("volume.write", {
            **target(volume), "name": "payload", "content": self.content,
        })["volume_ref"]
        self.params = {**target(volume), "name": "payload"}

    def read_range(self, reference, offset, length):
        page = self.n.call("Artifact.ReadChunk", dict(reference=reference, offset=offset, length=length))
        self.assertEqual(page["offset"], offset)
        end = min(page["total_bytes"], offset + length)
        data = base64.b64decode(page["data_base64"], validate=True)
        self.assertEqual(len(data), end - offset)
        self.assertEqual(page["next_offset"], end if end < page["total_bytes"] else None)
        return data

    def reject_chunk_inserts(self, index):
        self.n.stop()
        with closing(sqlite3.connect(self.n.state / "state.db")) as db, db:
            count, = db.execute("SELECT COUNT(*) FROM artifact_chunks").fetchone()
            db.execute(f"""
                CREATE TRIGGER fail_chunk BEFORE INSERT ON artifact_chunks
                WHEN NEW.chunk_index={index} BEGIN
                    SELECT RAISE(ABORT, 'test result persistence failure');
                END
            """)
        self.n.start()
        return count

    def restore_chunk_inserts(self, original_count):
        self.n.stop()
        with closing(sqlite3.connect(self.n.state / "state.db")) as db, db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM artifact_chunks").fetchone(), (original_count,))
            db.execute("DROP TRIGGER fail_chunk")
        self.n.start()

    def test_range_crosses_chunks_and_unrelated_damage_does_not_read_other_chunks(self):
        n = self.n
        task = n.wait(n.submit("volume.read", self.params))
        self.assertEqual((task["lifecycle"], task["result"]["availability"]), ("Succeeded", "Available"))
        reference, = task["result"]["artifact_refs"]
        cli = [n.binaries / "psiosctl", "--config", n.config, "artifact"]
        stored = subprocess.check_output(cli, input=json.dumps(reference).encode(), timeout=20)
        self.assertGreater(len(stored), 2 * CHUNK)
        self.assertEqual(reference["digest"], "sha256:" + hashlib.sha256(stored).hexdigest())
        expected = {"name": "payload", "content": self.content}
        self.assertEqual(json.loads(stored), expected)
        self.assertEqual(n.call("Artifact.Read", {"reference": reference}), expected)
        resolved = n.call("Resource.Resolve", {"reference": reference})
        self.assertEqual((resolved["reference"], resolved["value"]), (reference, expected))
        self.assertEqual(n.get(reference["resource_id"]), resolved)
        spec = resolved["resource"]["spec"]
        self.assertEqual((spec["task_id"], spec["run_id"]), (task["task_id"], task["current_run_id"]))
        self.assertEqual((spec["size_bytes"], spec["digest"]), (str(len(stored)), reference["digest"]))
        cross = (CHUNK - 7, 19)
        self.assertEqual(self.read_range(reference, *cross), stored[cross[0]:cross[0] + cross[1]])

        n.stop(signal.SIGKILL)
        verified = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertEqual(verified.returncode, 0, verified.stderr)
        n.start()
        self.assertEqual(self.read_range(reference, *cross), stored[cross[0]:cross[0] + cross[1]])
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload))"
                       " WHERE artifact_id=? AND chunk_index=1", (reference["resource_id"],))
        corrupt = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(corrupt.returncode, 0)
        self.assertIn("artifact chunk digest mismatch", corrupt.stderr)

        n.start()
        self.assertEqual(self.read_range(reference, 0, 20), stored[:20])
        self.assertEqual(self.read_range(reference, 2 * CHUNK, 20), stored[2 * CHUNK:2 * CHUNK + 20])
        for method, args in [
            ("Artifact.ReadChunk", {"offset": cross[0], "length": cross[1]}),
            ("Artifact.Read", {}),
            ("Resource.Resolve", {}),
        ]:
            with self.subTest(method=method):
                with self.assertRaises(Failure) as error:
                    n.call(method, {"reference": reference, **args})
                self.assertEqual(error.exception.code, "RecoveryRequired")
        settled = n.call("Task.Get", {"id": task["task_id"]})
        self.assertEqual((settled["lifecycle"], settled["result"]["artifact_refs"]), ("Succeeded", [reference]))

    def test_second_chunk_write_failure_rolls_back_manifest_and_success(self):
        n = self.n
        before = n.call("Resource.List", {"kind": "Artifact"})["items"]
        chunks_before = self.reject_chunk_inserts(1)
        request = n.prepare("volume.read", self.params, "fail-result-commit")
        receipt = n.call("Task.Submit", request)
        tid = receipt["task_id"]
        failed = n.wait(tid)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "Known"))
        self.assertEqual(failed["conditions"][0]["type"], "StorageUnavailable")
        self.assertEqual(failed["result"], {"availability": "Unavailable", "artifact_refs": []})
        self.assertEqual(n.call("Resource.List", {"kind": "Artifact"})["items"], before)
        self.restore_chunk_inserts(chunks_before)
        replay = n.call("Task.Submit", request)
        self.assertTrue(replay["deduplicated"])
        self.assertEqual(replay["task_id"], tid)
        self.assertEqual(n.call("Task.Get", {"id": tid}), failed)
        self.assertEqual(n.perform("volume.read", self.params)["content"], self.content)

    def test_lost_write_result_keeps_the_hold_until_the_original_effect_is_reconciled(self):
        n = self.n
        chunks_before = self.reject_chunk_inserts(0)
        request = n.prepare("volume.write", {**self.params, "content": "observed effect"}, "lost-write-result")
        tid = n.call("Task.Submit", request)["task_id"]
        failed = n.wait(tid, autoapprove=True)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Paused", "Unknown"))
        self.assertEqual(failed["result"], {"availability": "Unavailable", "artifact_refs": []})
        data = n.state / "volumes" / self.params["id"] / "data" / self.params["name"]
        self.assertEqual(data.read_text(), "observed effect")
        before = data.stat()
        self.restore_chunk_inserts(chunks_before)
        held = n.wait(n.submit("volume.read", self.params))
        self.assertEqual(held["conditions"][0]["type"], "Conflict")
        self.assertTrue(n.call("Task.Submit", request)["deduplicated"])
        operation, = failed["operation_refs"]
        n.call("Operation.Reconcile", target(operation))
        restored = n.wait(tid)
        self.assertEqual((restored["lifecycle"], restored["outcome_knowledge"]), ("Succeeded", "Known"))
        reference, = restored["result"]["artifact_refs"]
        evidence = n.call("Resource.Resolve", {"reference": reference})
        self.assertEqual(evidence["resource"]["spec"]["operation_id"], operation["resource_id"])
        self.assertEqual(evidence["value"]["receipt"]["digest"],
                         "sha256:" + hashlib.sha256(b"observed effect").hexdigest())
        after = data.stat()
        self.assertEqual((after.st_ino, after.st_mtime_ns), (before.st_ino, before.st_mtime_ns))
        self.assertEqual(n.perform("volume.read", {
            **target(evidence["value"]["volume_ref"]), "name": self.params["name"],
        })["content"], "observed effect")
