"""Exercise offline capture, independent verification and live result repair."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import hashlib
import json
import os
import shutil
import signal
import sqlite3
import stat
import subprocess
import unittest

from support import Failure, Node, target


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def files(roots):
    return {
        p: (p.stat().st_ino, p.stat().st_mtime_ns, stat.S_IMODE(p.stat().st_mode), digest(p.read_bytes()))
        for root in roots for p in root.rglob("*") if p.is_file()
    }


class AuthorityCheckpoint(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.exports = self.n.base / "exports"
        self.scratch = self.n.base / "scratch"
        for directory in (self.exports, self.scratch):
            directory.mkdir(mode=0o700)
        self.archive = self.exports / "cut"

    def capture(self, *args, output=None):
        return subprocess.run(self.n.daemon + ["checkpoint", "--output", output or self.archive, *args],
                              capture_output=True, timeout=20)

    def detached(self, command, expected, *args, directory=None, reference=None):
        result = subprocess.run([
            self.n.binaries / "psiosd", command, "--directory", directory or self.archive,
            "--manifest-digest", expected, "--scratch-dir", self.scratch, *args,
        ], input=json.dumps(reference).encode() if reference else None, capture_output=True, timeout=20)
        self.assertEqual(list(self.scratch.iterdir()), [])
        return result

    def successful(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def refused(self, code, result):
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"")
        self.assertIn(code.encode(), result.stderr)

    def test_crash_cut_recovers_original_result_without_rolling_back_live_state(self):
        n = self.n
        node_ref = n.call("Node.Get", {})["reference"]
        content = "\0" * (48 * 1024) + "尾"
        volume = n.perform("volume.create", {"name": "checkpoint-result"})
        volume = n.perform("volume.write", {
            **target(volume), "name": "payload", "content": content,
        })["volume_ref"]
        task = n.wait(n.submit("volume.read", {**target(volume), "name": "payload"}))
        self.assertEqual(task["lifecycle"], "Succeeded")
        reference, = task["result"]["artifact_refs"]
        self.refused("Conflict", self.capture())
        self.assertEqual(list(self.exports.iterdir()), [])
        n.stop(signal.SIGKILL)
        self.assertGreater((n.state / "state.db-wal").stat().st_size, 32)
        head = n.witness / "head"
        trusted_head = head.read_bytes()
        head.write_text(json.dumps({"committed": "unrelated"}))
        before = files((n.state, n.witness))
        self.refused("RecoveryRequired", self.capture())
        self.assertEqual(files((n.state, n.witness)), before)
        self.assertEqual(list(self.exports.iterdir()), [])
        head.write_bytes(trusted_head)
        before = files((n.state, n.witness))
        report = self.successful(self.capture())
        self.assertEqual(files((n.state, n.witness)), before)

        expected = report["manifest_digest"]
        manifest = report["manifest"]
        identity = manifest["identity"]
        self.assertEqual(identity["node_id"], node_ref["resource_id"])
        self.assertEqual(identity["head"], json.loads(trusted_head)["committed"])
        self.assertEqual(manifest["witness_at_capture"], "Confirmed")
        self.assertEqual(stat.S_IMODE(self.archive.stat().st_mode), 0o700)
        self.assertEqual(expected, digest((self.archive / "manifest.json").read_bytes()))
        self.assertEqual({p.name for p in self.archive.iterdir()}, {"manifest.json", *manifest["members"]})
        self.assertIn("state.db", manifest["members"])
        self.assertLessEqual(set(manifest["members"]), {"state.db", "state.db-wal", "state.db-journal"})
        for name, member in manifest["members"].items():
            path = self.archive / name
            raw = path.read_bytes()
            self.assertEqual(member, {"bytes": str(len(raw)), "digest": digest(raw)})
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertNotIn(n.owner.encode(), raw)

        archive_before = files((self.archive,))
        moved = [(path, path.with_suffix(".offline")) for path in (n.state, n.witness, n.config)]
        for path, dest in moved:
            path.rename(dest)
        try:
            self.assertEqual(self.successful(self.detached("verify-checkpoint", expected)), report)
            self.refused("QuotaExceeded", self.detached(
                "checkpoint-artifact", expected, "--max-bytes", "1", reference=reference,
            ))
            self.refused("PreconditionFailed", self.detached(
                "checkpoint-artifact", expected, reference=dict(reference, digest="sha256:" + "0" * 64),
            ))
            self.refused("InvalidArgument", self.detached("checkpoint-artifact", expected, reference=node_ref))
            extract = self.detached("checkpoint-artifact", expected, reference=reference)
            self.assertEqual(extract.returncode, 0, extract.stderr)
            data = extract.stdout
            self.assertEqual(digest(data), reference["digest"])
            self.assertEqual(json.loads(data), {"name": "payload", "content": content})
        finally:
            for path, dest in moved:
                dest.rename(path)
        self.assertEqual(files((self.archive,)), archive_before)

        n.start()
        later, _ = n.agent(["task.read"])
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            deleted = db.execute("DELETE FROM artifact_chunks WHERE artifact_id=? AND chunk_index=1",
                                 (reference["resource_id"],)).rowcount
            self.assertEqual(deleted, 1)
        damaged = self.exports / "damaged"
        self.refused("RecoveryRequired", self.capture(output=damaged))
        self.assertFalse(damaged.exists())
        n.start()
        with self.assertRaises(Failure) as failed:
            n.call("Artifact.Read", {"reference": reference})
        self.assertEqual(failed.exception.code, "RecoveryRequired")
        payload = self.exports / "result.json"
        payload.write_bytes(data)
        repair = subprocess.run([
            n.binaries / "psiosctl", "--config", n.config, "artifact-repair", payload,
        ], input=json.dumps(reference).encode(), capture_output=True, timeout=20)
        self.assertEqual(self.successful(repair), {
            "reference": reference, "size_bytes": str(len(data)), "chunks_repaired": 1,
        })
        self.assertEqual(n.call("Artifact.Read", {"reference": reference}), json.loads(data))
        self.assertEqual(n.call("Task.Get", {"id": task["task_id"]}), task)
        self.assertEqual(n.call("Principal.WhoAmI", {}, later["token"])["principal_id"], later["principal_id"])
        n.stop()
        verified = subprocess.run(n.daemon + ["verify"], capture_output=True, timeout=20)
        self.assertEqual(self.successful(verified)["witness"], "Confirmed")

    def test_atomic_publication_and_detached_refusal(self):
        self.n.stop()
        self.refused("InvalidArgument", self.capture(output=self.n.state / "inside"))
        with ThreadPoolExecutor(max_workers=2) as callers:
            outcomes = list(callers.map(lambda _: self.capture(), range(2)))
        accepted, = [result for result in outcomes if result.returncode == 0]
        refused, = [result for result in outcomes if result.returncode != 0]
        self.refused("Conflict", refused)
        report = self.successful(accepted)
        expected = report["manifest_digest"]
        self.assertEqual(list(self.exports.iterdir()), [self.archive])
        self.refused("Conflict", self.capture())
        self.assertEqual(self.successful(self.detached("verify-checkpoint", expected)), report)
        self.refused("PreconditionFailed", self.detached("verify-checkpoint", "sha256:" + "0" * 64))

        copy = self.exports / "tampered"
        shutil.copytree(self.archive, copy)
        member = copy / "state.db"
        original = member.read_bytes()
        member.write_bytes(bytes([original[0] ^ 1]) + original[1:])
        self.refused("PreconditionFailed", self.detached("verify-checkpoint", expected, directory=copy))
        member.write_bytes(original)
        extra = copy / "extra"
        extra.touch()
        self.refused("PreconditionFailed", self.detached("verify-checkpoint", expected, directory=copy))
        extra.unlink()
        member.unlink()
        self.refused("PreconditionFailed", self.detached("verify-checkpoint", expected, directory=copy))
        member.symlink_to(self.archive / "state.db")
        self.refused("StorageUnavailable", self.detached("verify-checkpoint", expected, directory=copy))
        alias = self.exports / "alias"
        alias.symlink_to(self.archive, target_is_directory=True)
        self.refused("InvalidArgument", self.detached("verify-checkpoint", expected, directory=alias))
        member.unlink()
        member.write_bytes(original)

        # A matching digest for an edited manifest still cannot speak for the SQLite identity.
        report["manifest"]["identity"]["head"] = "different-cut"
        raw = json.dumps(report["manifest"]).encode()
        (copy / "manifest.json").write_bytes(raw)
        self.refused("RecoveryRequired", self.detached("verify-checkpoint", digest(raw), directory=copy))
