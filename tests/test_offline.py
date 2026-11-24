"""Verify real crash state without checkpointing the source WAL or repairing its witness."""
import hashlib
import json
import os
import signal
import subprocess
import unittest

from support import Node


class OfflineVerification(unittest.TestCase):
    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)
        self.scratch = self.n.base / "scratch"
        self.scratch.mkdir(mode=0o700)

    def verify(self, *args, error=None):
        p = subprocess.run(self.n.daemon + ["verify", "--scratch-dir", self.scratch, *args],
                           capture_output=True, text=True, timeout=15)
        self.assertEqual(list(self.scratch.iterdir()), [])
        self.assertNotIn(self.n.owner, p.stdout + p.stderr)
        if error:
            self.assertNotEqual(p.returncode, 0)
            self.assertIn(error, p.stderr)
        else:
            self.assertEqual(p.returncode, 0, p.stderr)
            return json.loads(p.stdout)

    def contents(self):
        files = [p for root in (self.n.state, self.n.witness) for p in root.rglob("*") if p.is_file()]
        return {p: (p.stat().st_ino, p.stat().st_mtime_ns, hashlib.sha256(p.read_bytes()).digest()) for p in files}

    def test_crash_wal_and_pending_witness(self):
        n = self.n
        self.verify(error="Conflict")
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        n.call("Agent.Register", {"name": "before-crash"})
        n.stop(signal.SIGKILL)
        self.assertGreater((n.state / "state.db-wal").stat().st_size, 32)
        before = self.contents()
        result = self.verify()
        self.assertEqual((result["node_id"], result["database_format"], result["witness"]),
                         (node, 7, "Confirmed"))
        self.assertEqual(self.contents(), before)
        head = n.witness / "head"
        committed = json.loads(head.read_text())["committed"]
        for marker, expected in [({"committed": "prior", "pending": committed}, "PendingCommitFinalization"),
                                 ({"committed": committed, "pending": "uncommitted"}, "PendingIntentDiscard")]:
            head.write_text(json.dumps(marker))
            before = self.contents()
            self.assertEqual(self.verify()["witness"], expected)
            self.assertEqual(self.contents(), before)
        head.write_text(json.dumps({"committed": "unrelated"}))
        self.verify(error="RecoveryRequired")
        head.write_text(json.dumps({"committed": committed}))
        n.start()
        self.assertEqual(n.call("Node.Get", {})["reference"]["resource_id"], node)

    def test_owner_rotation_missing_lock_and_copy_limit(self):
        n = self.n
        n.stop()
        token, nxt = n.witness / "owner.token", n.witness / "owner.next"
        token.rename(nxt)
        self.assertEqual(self.verify()["owner_rotation"], "PendingPromotion")
        self.assertFalse(token.exists())
        nxt.rename(token)
        nxt.write_text("unused-rotation")
        self.assertEqual(self.verify()["owner_rotation"], "PendingDiscard")
        self.assertTrue(nxt.exists())
        token.write_text("wrong-token")
        self.verify(error="RecoveryRequired")
        token.write_text(n.owner)
        # A private copy of this enlarged database must respect the operator's cap.
        os.truncate(n.state / "state.db", 2 * 1024 * 1024)
        self.verify("--max-copy-mib", "1", error="QuotaExceeded")
        lock = n.state / "daemon.lock"
        lock.unlink()
        self.verify(error="StorageUnavailable")
        self.assertFalse(lock.exists())

    def test_witness_special_file_and_corruption(self):
        n = self.n
        n.stop()
        keys = n.witness / "keys"
        material = keys.read_bytes()
        keys.unlink()
        os.mkfifo(keys, 0o600)
        self.verify(error="RecoveryRequired")
        keys.unlink()
        keys.write_bytes(material)
        with (n.state / "state.db").open("r+b") as f:
            f.write(b"damaged sqlite header")
        before = self.contents()
        self.verify(error="StorageUnavailable")
        self.assertEqual(self.contents(), before)
