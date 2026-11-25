"""Exercise bounded reads and interrupted publication using the real authority."""
import base64
from concurrent.futures import ThreadPoolExecutor
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

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as failure:
            self.n.call(method, params, token)
        self.assertEqual(failure.exception.code, code, failure.exception)

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
        self.denied("PreconditionFailed", "Task.EvictResults", {
            "id": tid, "expected_resource_version": restored["task_revision"], "reason": "expired",
        })

    def test_eviction_is_atomic_audited_and_preserves_task_run_and_deduplication(self):
        n = self.n
        submission = n.prepare("volume.read", self.params, "retention-replay")
        tid = n.call("Task.Submit", submission)["task_id"]
        task = n.wait(tid)
        reference, = task["result"]["artifact_refs"]
        artifact = {"reference": reference}
        describe = n.call("Artifact.Get", artifact)
        self.assertEqual(describe["availability"], "Available")
        self.assertIsNone(describe["retention"])
        run_args = {"id": task["current_run_id"]}
        run = n.call("Run.Get", run_args)
        original = n.get(tid)["reference"]
        request = {**target(original), "reason": "expired test results"}
        agent, _ = n.agent(["task.read", "task.submit", "resource.read"])
        self.denied("PermissionDenied", "Task.EvictResults", request, agent["token"])
        self.denied("PermissionDenied", "Artifact.Get", artifact, agent["token"])
        self.denied("Conflict", "Task.EvictResults", dict(request, expected_resource_version="r1"))
        self.denied("InvalidArgument", "Task.EvictResults", dict(request, reason=" "))

        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("""
                CREATE TRIGGER fail_eviction BEFORE INSERT ON versions
                WHEN NEW.kind='TaskResultEviction' BEGIN
                    SELECT RAISE(ABORT, 'test eviction receipt failure');
                END
            """)
        n.start()
        self.denied("StorageUnavailable", "Task.EvictResults", request)
        self.assertEqual(n.call("Task.Get", {"id": tid}), task)
        self.assertEqual(n.call("Artifact.Read", artifact)["content"], self.content)
        self.assertIsNone(n.call("Artifact.Get", artifact)["retention"])
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER fail_eviction")
        n.start()

        with ThreadPoolExecutor(max_workers=4) as pool:
            receipts = list(pool.map(lambda _: n.call("Task.EvictResults", request), range(4)))
        receipt = receipts[0]
        self.assertTrue(all(result == receipt for result in receipts))
        data = receipt["eviction"]
        self.assertEqual((data["previous_task_ref"], data["artifact_refs"]), (original, [reference]))
        self.assertEqual(data["content_bytes_removed"], describe["resource"]["spec"]["size_bytes"])
        self.assertEqual(n.call("Artifact.Get", artifact)["retention"], receipt)
        self.assertEqual(n.get(tid)["reference"], data["task_ref"])
        self.assertEqual(n.call("Resource.Resolve", {"reference": original})["value"], task)
        self.assertEqual(n.call("Run.Get", run_args), run)
        self.assertEqual(n.call("Task.Get", {"id": tid})["result"],
                         {"availability": "Evicted", "artifact_refs": [reference]})
        evictions = [entry for entry in n.call("Audit.List", {"limit": 128})
                     if entry["method"] == "Task.EvictResults"]
        self.assertEqual([entry["resource"] for entry in evictions if entry["decision"] == "Evicted"], [tid])
        self.assertIn("PermissionDenied", [entry["decision"] for entry in evictions])

        n.stop(signal.SIGKILL)
        verified = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertEqual(verified.returncode, 0, verified.stderr)
        with closing(sqlite3.connect(n.state / "state.db")) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM artifact_chunks WHERE artifact_id=?",
                                        (reference["resource_id"],)).fetchone(), (0,))
        n.start()
        self.assertEqual(n.call("Task.EvictResults", request), receipt)
        self.denied("Conflict", "Task.EvictResults", dict(request, reason="a different reason"))
        self.denied("ResultEvicted", "Artifact.Read", artifact)
        self.denied("ResultEvicted", "Artifact.ReadChunk", dict(artifact, offset=0, length=1))
        self.denied("ResultEvicted", "Resource.Resolve", artifact)
        self.denied("PermissionDenied", "Artifact.Read", artifact, agent["token"])
        replay = n.call("Task.Submit", submission)
        self.assertTrue(replay["deduplicated"])
        self.assertEqual(replay["task_id"], tid)
        self.assertEqual(n.call("Task.Get", {"id": tid})["lifecycle"], "Succeeded")
        cli = [n.binaries / "psiosctl", "--config", n.config, "artifact"]
        exported = subprocess.run(cli, input=json.dumps(reference).encode(), capture_output=True, timeout=20)
        self.assertNotEqual(exported.returncode, 0)
        self.assertIn(b"ResultEvicted", exported.stderr)
        self.assertEqual(exported.stdout, b"")
