"""Verify real crash state without checkpointing the source WAL or repairing its witness."""
from contextlib import closing
import hashlib
import json
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Node, target


class OfflineVerification(unittest.TestCase):
    def setUp(self):
        self.n = Node()
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
        self.verify("--rehearse-migration", error="Conflict")
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        n.call("Agent.Register", {"name": "before-crash"})
        n.stop(signal.SIGKILL)
        self.assertGreater((n.state / "state.db-wal").stat().st_size, 32)
        before = self.contents()
        result = self.verify()
        self.assertEqual((result["node_id"], result["migration_required"], result["witness"]),
                         (node, False, "Confirmed"))
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
        original = self.contents()
        report = self.verify("--rehearse-migration")
        self.assertEqual(report["migration_rehearsal"], {
            "status": "Verified", "source_format": report["database_format"],
            "target_format": report["database_format"]})
        self.assertEqual(self.contents(), original)
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
        self.verify("--rehearse-migration", "--max-copy-mib", "1", error="QuotaExceeded")
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

    def test_offline_vault_authenticates_active_secrets_before_checkpoint_publication(self):
        n = self.n
        first = "first synthetic vault fixture"
        current = "rotated synthetic vault fixture"
        secret = n.call("Secret.Put", {"name": "active-key", "value": first})
        rotated = n.call("Secret.Rotate", {**target(secret), "value": current})
        revoked = n.call("Secret.Put", {"name": "retired-key", "value": "retired fixture"})
        n.call("Secret.Revoke", target(revoked))
        n.stop(signal.SIGKILL)
        before = self.contents()
        self.assertEqual(self.verify()["secret_vault"], "Verified")
        self.assertEqual(self.verify("--rehearse-migration")["secret_vault"], "Verified")
        self.assertEqual(self.contents(), before)

        clean = n.base / "clean-checkpoint"
        captured = subprocess.run([*n.daemon, "checkpoint", "--output", clean],
                                  capture_output=True, text=True, timeout=15)
        self.assertEqual(captured.returncode, 0, captured.stderr)
        self.assertNotIn(first, captured.stdout + captured.stderr)
        self.assertNotIn(current, captured.stdout + captured.stderr)
        self.assertFalse((clean / "keys").exists())
        for file in clean.iterdir():
            if file.is_file():
                contents = file.read_bytes()
                self.assertFalse(first.encode() in contents or current.encode() in contents)

        identifier = rotated["resource_id"]
        database = n.state / "state.db"
        with closing(sqlite3.connect(database)) as db, db:
            nonce, sealed = db.execute("SELECT nonce,ciphertext FROM secrets WHERE id=?",
                                       (identifier,)).fetchone()
            db.execute("UPDATE secrets SET ciphertext=zeroblob(length(ciphertext)) WHERE id=?",
                       (identifier,))
        present = self.contents()
        self.verify(error="vault authentication failed")
        self.assertEqual(self.contents(), present)
        rejected = n.base / "damaged-checkpoint"
        outcome = subprocess.run([*n.daemon, "checkpoint", "--output", rejected],
                                 capture_output=True, text=True, timeout=15)
        self.assertNotEqual(outcome.returncode, 0)
        self.assertIn("vault authentication failed", outcome.stderr)
        self.assertNotIn(current, outcome.stderr + outcome.stdout)
        self.assertFalse(rejected.exists())

        with closing(sqlite3.connect(database)) as db, db:
            db.execute("UPDATE secrets SET ciphertext=zeroblob(100000) WHERE id=?", (identifier,))
        self.verify(error="active Secret has no valid sealed payload")
        with closing(sqlite3.connect(database)) as db, db:
            db.execute("UPDATE secrets SET ciphertext=? WHERE id=?", (sealed, identifier))
            db.execute("INSERT INTO secrets VALUES(?,zeroblob(12),zeroblob(17))", (revoked["resource_id"],))
        self.verify(error="Secret lifecycle disagrees with vault")
        with closing(sqlite3.connect(database)) as db, db:
            db.execute("DELETE FROM secrets WHERE id=?", (revoked["resource_id"],))
            db.execute("DELETE FROM secrets WHERE id=?", (identifier,))
        self.verify(error="active Secret has no valid sealed payload")
        with closing(sqlite3.connect(database)) as db, db:
            db.execute("INSERT INTO secrets VALUES(?,?,?)", (identifier, nonce, sealed))
        self.assertEqual(self.verify()["secret_vault"], "Verified")
