"""Compact real evicted result pages while preserving the offline authority."""
import base64
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
from select import select
import shutil
import signal
import stat
import struct
import subprocess
import sys
from time import monotonic, sleep
import unittest

from support import Failure, Node, target
from test_task_result_eviction_batch import APPLY, PREPARE


MIB = 1024 * 1024
CHUNK = 96 * 1024
IDENTITY = ("node_id", "recovery_incarnation", "head", "database_format")
GEOMETRY = ("main_bytes", "wal_bytes", "page_size", "page_count", "free_pages")


@dataclass(frozen=True)
class Read:
    request: dict
    accepted: dict
    task_ref: dict
    artifact_ref: dict
    content_bytes: int


def fingerprint(path):
    details = path.stat()
    with path.open("rb") as file:
        digest = hashlib.file_digest(file, "sha256").hexdigest()
    return (details.st_ino, details.st_mtime_ns, stat.S_IMODE(details.st_mode), details.st_size, digest)


def wal_commit_pages(path, page_size):
    # Watch the final complete frame of this initially empty WAL. SQLite's
    # later offline integrity check independently validates the full checksums.
    try:
        with path.open("rb", buffering=0) as file:
            size = os.fstat(file.fileno()).st_size
            frame = 24 + page_size
            if size < 32 + frame or (size - 32) % frame:
                return None
            header = file.read(32)
            if len(header) != 32:
                return None
            magic, _, size_in_header = struct.unpack_from(">III", header)
            if magic not in (0x377F0682, 0x377F0683) or size_in_header != page_size:
                return None
            file.seek(size - frame)
            tail = file.read(24)
            if len(tail) != 24 or tail[8:16] != header[16:24]:
                return None
            page, committed = struct.unpack_from(">II", tail)
            if page and committed:
                return committed
    except FileNotFoundError:
        return None
    return None


class StateCompactionTests(unittest.TestCase):
    def setUp(self):
        self.current = Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).absolute() / "psiosd"
        self.node = Node(os.getenv("PSIOS_TEST_COMPACTION_PREVIOUS") or self.current.parent)
        self.addCleanup(self.node.close)
        self.scratch = self.node.base / "compact-scratch"
        self.scratch.mkdir(mode=0o700)

    def invocation(self, action, *args, maximum=32, node=None):
        return [self.current, "--config", (node or self.node).config, action,
                "--scratch-dir", self.scratch, "--max-copy-mib", str(maximum), *args]

    def command(self, action, *args, input=None, maximum=32, node=None):
        result = subprocess.run(self.invocation(action, *args, maximum=maximum, node=node),
                                input=input, capture_output=True, timeout=35)
        self.assertEqual(list(self.scratch.iterdir()), [], result.stderr)
        return result

    def success(self, result):
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        return json.loads(result.stdout)

    def refused(self, code, result):
        self.assertNotEqual(result.returncode, 0, result.stdout.decode())
        self.assertFalse(result.stdout, result)
        self.assertIn(code.encode(), result.stderr, result.stderr.decode())

    def plan(self, **options):
        envelope = self.success(self.command("plan-state-compaction", **options))
        self.assertEqual(set(envelope), {"plan", "plan_digest"}, envelope)
        self.assertRegex(envelope["plan_digest"], r"^sha256:[0-9a-f]{64}$")
        return envelope

    def apply(self, envelope, *, filename=None, **options):
        if filename is not None:
            filename.write_text(json.dumps(envelope))
            filename.chmod(0o600)
            return self.command("compact-state", "--plan-file", filename, **options)
        return self.command("compact-state", "--plan-file", "-", input=json.dumps(envelope).encode(), **options)

    def immutable_files(self):
        n = self.node
        paths = (n.state / name for name in ("state.db", "state.db-wal", "state.db-journal"))
        return {path: fingerprint(path) for path in (*paths, *n.witness.iterdir()) if path.is_file()}

    def witness(self):
        return {path: fingerprint(path) for path in self.node.witness.iterdir() if path.is_file()}

    def read(self, volume, payload, offset, expected):
        n = self.node
        request = n.prepare("volume.read_chunk", {
            "target": target(volume), "path": payload.name, "offset": offset, "limit": len(expected),
        }, submission_key=f"compaction-read-{offset}")
        accepted = n.call("Task.Submit", request)
        state = n.wait(accepted["task_id"])
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"]), ("Succeeded", "Known"), state)
        artifact, = state["result"]["artifact_refs"]
        result = n.call("Artifact.Read", {"reference": artifact})
        self.assertEqual(base64.b64decode(result["data_base64"], validate=True), expected)
        info = n.call("Artifact.Get", {"reference": artifact})
        return Read(request, accepted, n.get(accepted["task_id"])["reference"], artifact,
                    int(info["resource"]["spec"]["size_bytes"]))

    def evict(self, reads):
        n = self.node
        total = sum(read.content_bytes for read in reads)
        plan = n.call(PREPARE, {"task_refs": [read.task_ref for read in reads],
                               "reason": "explicitly retain only the final observed chunk", "max_content_bytes": total})
        self.assertEqual(plan["content_bytes"], total, plan)
        receipt = n.call(APPLY, {"plan_token": plan["plan_token"]})
        self.assertEqual(receipt["batch"]["content_bytes_removed"], str(total), receipt)
        self.assertEqual(len(receipt["batch"]["eviction_refs"]), len(reads), receipt)
        return receipt

    def evicted(self, artifact, retention):
        n = self.node
        record = n.call("Artifact.Get", {"reference": artifact})
        self.assertEqual(record["availability"], "Evicted", record)
        self.assertEqual(record["retention"], retention, record)
        for method, fields in (("Artifact.Read", {}), ("Artifact.ReadChunk", {"offset": 0, "length": 1})):
            with self.assertRaises(Failure) as rejected:
                n.call(method, {"reference": artifact, **fields})
            self.assertEqual(rejected.exception.code, "ResultEvicted", rejected.exception)

    def make_reclaimable(self, batch_count=2):
        n = self.node
        volume = n.perform("volume.create", {"name": "authority-compaction-input"})
        payload = n.state / "volumes" / volume["resource_id"] / "data" / "chunks"
        data = os.urandom((16 * batch_count + 1) * CHUNK)
        payload.write_bytes(data)
        # Real storage Tasks create chunked Artifact bytes and durable receipt
        # history. Only the explicit retention API removes the selected payload.
        observations = [self.read(volume, payload, pos, data[pos:pos + CHUNK])
                        for pos in range(0, len(data), CHUNK)]
        batches = [self.evict(observations[16 * i:16 * (i + 1)]) for i in range(batch_count)]
        self.assertGreater(sum(int(batch["batch"]["content_bytes_removed"]) for batch in batches), 2 * MIB * batch_count)
        return observations[0], observations[-1], batches, payload, hashlib.sha256(data).hexdigest()

    def test_offline_compaction_reclaims_real_results_without_changing_authority(self):
        n = self.node
        initial = n.call("Principal.WhoAmI", {})["authority_id"]
        first, kept, batches, payload, payload_digest = self.make_reclaimable()
        retention = n.call("Artifact.Get", {"reference": first.artifact_ref})["retention"]
        self.evicted(first.artifact_ref, retention)
        current_evicted_task = n.get(first.accepted["task_id"])
        kept_result = n.call("Artifact.Read", {"reference": kept.artifact_ref})
        kept_task = n.get(kept.accepted["task_id"])

        n.stop()
        frozen = self.immutable_files()
        stale = self.plan()
        self.assertEqual(self.immutable_files(), frozen)
        self.assertEqual(stale["plan"]["node_id"], initial)
        self.assertGreater(stale["plan"]["free_pages"] * stale["plan"]["page_size"], 2 * MIB, stale)

        n.start()
        self.refused("Conflict", self.command("plan-state-compaction"))
        self.refused("Conflict", self.apply(stale, filename=n.base / "stale-compaction-plan.json"))
        latest = n.perform("system.status", {})
        self.assertEqual(latest["authority_id"], initial)
        n.stop()
        frozen = self.immutable_files()
        self.refused("Conflict", self.apply(stale))
        self.refused("QuotaExceeded", self.command("plan-state-compaction", maximum=1))
        ready = self.plan()
        self.assertNotEqual(ready["plan"]["head"], stale["plan"]["head"])
        wrong = {**ready, "plan_digest": "sha256:" + "0" * 64}
        self.refused("InvalidArgument", self.apply(wrong))
        other = Node(n.binaries)
        try:
            other.stop()
            other_plan = self.plan(node=other)
            self.assertNotEqual(other_plan["plan"]["node_id"], initial)
            self.refused("Conflict", self.apply(other_plan))
        finally:
            other.close()
        self.assertEqual(self.immutable_files(), frozen)

        original_config = n.config.read_bytes()
        available = os.statvfs(n.state)
        floor = available.f_bavail * available.f_frsize + MIB
        n.config.write_text(json.dumps({**json.loads(original_config), "authority_reserve": {"state_bytes": floor}}))
        try:
            self.refused("QuotaExceeded", self.apply(ready))
            self.assertEqual(self.immutable_files(), frozen)
        finally:
            n.config.write_bytes(original_config)

        # A separately held SQLite writer reservation must fail without
        # modifying the stopped source. It is not the daemon's advisory lock.
        writer_code = """
import sqlite3, sys

with sqlite3.connect(sys.argv[1], isolation_level=None, timeout=2) as connection:
    connection.execute('BEGIN IMMEDIATE')
    print('held', flush=True)
    sys.stdin.readline()
    connection.execute('ROLLBACK')
"""
        with subprocess.Popen([sys.executable, "-B", "-c", writer_code, n.state / "state.db"],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as writer:
            try:
                self.assertTrue(select([writer.stdout], [], [], 5)[0], "SQLite fixture writer did not start")
                self.assertEqual(writer.stdout.readline(), "held\n", "SQLite fixture writer did not acquire its lock")
                locked = self.plan()
                with_writer = self.immutable_files()
                self.refused("Conflict", self.apply(locked))
                self.assertEqual(self.immutable_files(), with_writer)
            finally:
                _, writer_error = writer.communicate("release\n", timeout=5)
            self.assertEqual(writer.returncode, 0, writer_error)

        ready = self.plan()
        pre = ready["plan"]
        witness = self.witness()
        self.assertEqual(pre["head"], json.loads((n.witness / "head").read_text())["committed"])
        result = self.success(self.apply(ready))
        self.assertEqual(result["outcome"], "Compacted")
        self.assertTrue(result["vacuum_performed"], result)
        self.assertEqual(result["plan_digest"], ready["plan_digest"], result)
        self.assertEqual({field: result[field] for field in IDENTITY}, {field: pre[field] for field in IDENTITY})
        before, after = result["before"], result["after"]
        self.assertEqual({field: before[field] for field in GEOMETRY}, {field: pre[field] for field in GEOMETRY})
        self.assertEqual((after["free_pages"], after["wal_bytes"]), (0, 0), result)
        self.assertEqual(after["main_bytes"], (n.state / "state.db").stat().st_size, result)
        self.assertLess(after["page_count"], before["page_count"], result)
        reclaimed = before["main_bytes"] + before["wal_bytes"] - after["main_bytes"] - after["wal_bytes"]
        self.assertEqual(result["reclaimed_file_bytes"], reclaimed, result)
        self.assertEqual(result["reclaimed_allocated_bytes"],
                         max(0, before["allocated_file_bytes"] - after["allocated_file_bytes"]), result)
        self.assertGreater(reclaimed, MIB)
        self.assertEqual(self.witness(), witness)
        self.refused("Conflict", self.apply(ready))

        post = self.plan()
        compact_files = self.immutable_files()
        again = self.success(self.apply(post))
        self.assertEqual(again["outcome"], "AlreadyCompact")
        self.assertFalse(again["vacuum_performed"], again)
        self.assertEqual(again["reclaimed_file_bytes"], 0, again)
        self.assertEqual(again["before"], again["after"], again)
        self.assertEqual(self.immutable_files(), compact_files)
        verified = self.success(self.command("verify"))
        self.assertEqual(verified["node_id"], initial)
        self.assertEqual(verified["database_format"], pre["database_format"])
        self.assertEqual((verified["sqlite_integrity"], verified["foreign_keys"], verified["witness"]),
                         ("Verified", "Verified", "Confirmed"), verified)
        self.assertEqual(self.witness(), witness)

        n.start()
        self.assertEqual(n.call("Principal.WhoAmI", {})["authority_id"], initial)
        self.assertEqual(fingerprint(payload)[-1], payload_digest)
        self.assertEqual(n.get(kept.accepted["task_id"]), kept_task)
        self.assertEqual(n.call("Artifact.Read", {"reference": kept.artifact_ref}), kept_result)
        self.assertEqual(n.get(first.accepted["task_id"]), current_evicted_task)
        for read in (first, kept):
            self.assertEqual(n.call("Task.Submit", read.request), {**read.accepted, "deduplicated": True})
        self.evicted(first.artifact_ref, retention)
        for batch in batches:
            self.assertEqual(n.call("Resource.Resolve", {"reference": batch["reference"]})["value"], batch["batch"])
        n.stop()
        self.assertEqual(self.success(self.command("verify"))["node_id"], initial)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_STATE_COMPACTION_CUT") == "1" and shutil.which("strace"),
                         "requires strace syscall delay injection")
    def test_sigkill_after_compacted_wal_commit_preserves_authority_and_retries(self):
        n = self.node
        evicted, kept, _, payload, payload_digest = self.make_reclaimable(batch_count=1)
        expected = n.call("Artifact.Read", {"reference": kept.artifact_ref})
        retention = n.call("Artifact.Get", {"reference": evicted.artifact_ref})["retention"]
        n.stop()
        plan = self.plan()
        before = plan["plan"]
        self.assertEqual(before["wal_bytes"], 0, before)
        self.assertGreater(before["free_pages"], 0, before)
        main = n.state / "state.db"
        wal = n.state / "state.db-wal"
        source = fingerprint(main)
        witness = self.witness()
        approval = n.base / "compaction-cut-plan.json"
        approval.write_text(json.dumps(plan))
        approval.chmod(0o600)
        trace = n.base / "compaction-cut.strace"
        # Widen the normal sync boundary only. Select this specific write
        # using the actual WAL inode and complete commit frame at runtime.
        command = ["strace", "-D", "-qq", "-yy", "-s", "0", "-o", trace,
                   "-e", "trace=pwrite64,fdatasync,fsync,ftruncate",
                   "-e", "inject=fdatasync:delay_exit=700ms", "-e", "inject=fsync:delay_exit=700ms",
                   "--", *self.invocation("compact-state", "--plan-file", approval)]
        committed = None
        with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE) as process:
            try:
                deadline = monotonic() + 25
                while process.poll() is None and monotonic() < deadline:
                    pages = wal_commit_pages(wal, before["page_size"])
                    if pages is not None and pages < before["page_count"]:
                        proc = Path(f"/proc/{process.pid}")
                        try:
                            self.assertTrue(os.path.samefile(proc / "exe", self.current))
                            opened = [descriptor.readlink() for descriptor in (proc / "fd").iterdir()
                                      if descriptor.exists()]
                        except FileNotFoundError:
                            continue
                        if wal in opened:
                            committed = pages
                            break
                    sleep(0.003)
            finally:
                if process.poll() is None:
                    process.kill()
            stdout, stderr = process.communicate(timeout=10)
        self.assertIsNotNone(committed, (stdout, stderr, "no live committed compaction WAL observed"))
        self.assertEqual(process.returncode, -signal.SIGKILL, (stdout, stderr))
        self.assertFalse(stdout, stdout)
        wal_trace = [line for line in trace.read_text().splitlines() if f"<{wal}>" in line]
        writes = [index for index, line in enumerate(wal_trace) if "pwrite64(" in line]
        self.assertTrue(writes, wal_trace)
        self.assertFalse(any("ftruncate(" in line and ", 0)" in line for line in wal_trace[writes[-1]:]), wal_trace)
        self.assertEqual(fingerprint(main), source, "checkpoint already changed the main database")
        self.assertGreater(wal.stat().st_size, 32)
        self.assertEqual(self.witness(), witness)

        checked = self.success(self.command("verify"))
        self.assertEqual((checked["node_id"], checked["database_format"], checked["witness"]),
                         (before["node_id"], before["database_format"], "Confirmed"), checked)
        fresh = self.plan()
        recovered = fresh["plan"]
        self.assertEqual({field: recovered[field] for field in IDENTITY}, {field: before[field] for field in IDENTITY})
        self.assertGreater(recovered["wal_bytes"], 32, recovered)
        self.assertEqual((recovered["page_count"], recovered["free_pages"]), (committed, 0), recovered)
        self.refused("Conflict", self.apply(plan))
        settled = self.success(self.apply(fresh))
        self.assertEqual(settled["outcome"], "Compacted", settled)
        self.assertFalse(settled["vacuum_performed"], settled)
        self.assertEqual((settled["after"]["wal_bytes"], settled["after"]["page_count"]), (0, committed), settled)
        self.assertEqual(self.witness(), witness)

        n.start()
        self.assertEqual(n.call("Principal.WhoAmI", {})["authority_id"], before["node_id"])
        self.assertEqual(n.call("Artifact.Read", {"reference": kept.artifact_ref}), expected)
        self.assertEqual(n.call("Task.Submit", kept.request), {**kept.accepted, "deduplicated": True})
        self.evicted(evicted.artifact_ref, retention)
        self.assertEqual(fingerprint(payload)[-1], payload_digest)
        n.stop()
        self.assertEqual(self.success(self.command("verify"))["witness"], "Confirmed")


if __name__ == "__main__":
    unittest.main()
