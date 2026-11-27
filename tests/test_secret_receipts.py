"""Exercise transactional Secret mutations, abandoned responses, and offline evidence."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import signal
import socket
import sqlite3
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target


class SecretReceipts(unittest.TestCase):
    def setUp(self):
        self.node = Node()
        self.addCleanup(self.node.close)
        self.database = self.node.state / "state.db"
        self.scratch = self.node.base / "scratch"
        self.scratch.mkdir(mode=0o700)

    def query(self, sql, parameters=()):
        with closing(sqlite3.connect(self.database)) as db:
            return db.execute(sql, parameters).fetchall()

    def assert_denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as error:
            self.node.call(method, params, token)
        self.assertEqual(error.exception.code, code)

    def abandon_response(self, method, params):
        request = json.dumps(dict(schema_version="1.0.0", token=self.node.owner,
                                  request_id=key(), method=method, params=params)).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(10)
            stream.connect(str(self.node.socket))
            stream.sendall(struct.pack(">I", len(request)) + request)

    def wait_for_receipts(self, count):
        for _ in range(200):
            if self.query("SELECT count(*) FROM secret_mutations") == [(count,)]:
                return
            time.sleep(.025)
        self.fail("Secret mutation receipt did not commit")

    def test_committed_response_loss_replays_original_revision_without_mutating(self):
        n = self.node
        put = dict(name="fixture", value="  synthetic first\n\0", idempotency_key=key())
        self.abandon_response("Secret.Put", put)
        self.wait_for_receipts(1)
        n.stop(signal.SIGKILL)
        n.start()

        first = n.call("Secret.Put", put)
        rotate = {**target(first), "value": "synthetic second\n", "idempotency_key": key()}
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: n.call("Secret.Rotate", rotate), range(4)))
        second = results[0]
        self.assertEqual(results, [second] * 4)
        self.assertNotEqual(first["version"], second["version"])
        self.assertEqual(first["resource_id"], second["resource_id"])
        self.assertEqual(n.call("Secret.Put", put), first)
        self.assert_denied("IdempotencyConflict", "Secret.Put", dict(put, value="different bytes"))
        self.assert_denied("IdempotencyConflict", "Secret.Put", dict(put, name="different target"))
        self.assert_denied("IdempotencyConflict", "Secret.Rotate",
                           dict(rotate, expected_resource_version=second["version"]))
        self.assert_denied("IdempotencyConflict", "Secret.Revoke",
                           {**target(first), "idempotency_key": put["idempotency_key"]})
        self.assert_denied("Conflict", "Secret.Rotate", dict(rotate, idempotency_key=key()))

        revoke = {**target(second), "idempotency_key": key()}
        self.abandon_response("Secret.Revoke", revoke)
        self.wait_for_receipts(3)
        n.stop(signal.SIGKILL)
        n.start()
        revoked = n.get(first["resource_id"])
        self.assertEqual(n.call("Secret.Revoke", revoke), revoked["reference"])
        self.assertEqual(n.call("Secret.Rotate", rotate), second)
        self.assertEqual(revoked["value"]["lifecycle"], "Revoked")
        self.assertEqual(self.query("SELECT id,count(*) FROM versions WHERE kind='Secret' GROUP BY id"),
                         [(first["resource_id"], 3)])
        self.assertEqual(self.query("SELECT count(*) FROM secrets"), [(0,)])
        allowed = self.query(
            "SELECT method,resource FROM audit WHERE method LIKE 'Secret.%' AND decision='Allowed' ORDER BY seq")
        self.assertEqual(allowed, [(method, first["resource_id"])
                                  for method in ("Secret.Put", "Secret.Rotate", "Secret.Revoke")])
        self.assertEqual(self.query("SELECT DISTINCT length(lookup_tag),length(request_tag),length(receipt_tag)"
                                    " FROM secret_mutations"), [(32, 32, 32)])

    def test_keys_are_principal_local_and_current_permissions_still_apply(self):
        n = self.node
        agent, grant = n.agent(["secret.manage"])
        request = dict(name="actor-specific", value="synthetic shared request", idempotency_key=key())
        owner = n.call("Secret.Put", request)
        delegated = n.call("Secret.Put", request, agent["token"])
        self.assertNotEqual(owner["resource_id"], delegated["resource_id"])
        self.assertEqual(n.call("Secret.Put", request, agent["token"]), delegated)
        self.assert_denied("PermissionDenied", "Secret.Revoke",
                           {**target(owner), "idempotency_key": key()}, agent["token"])
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assert_denied("PermissionDenied", "Secret.Put", request, agent["token"])
        self.assertEqual(n.call("Secret.Put", request), owner)
        self.assertEqual(self.query("SELECT count(*) FROM secret_mutations"), [(2,)])

    def test_receipt_failure_rolls_back_and_a_damaged_receipt_blocks_checkpoint(self):
        n = self.node
        put = dict(name="rollback", value="synthetic private receipt fixture", idempotency_key=key())
        self.query("CREATE TRIGGER fail_receipt BEFORE INSERT ON secret_mutations "
                   "BEGIN SELECT RAISE(ABORT,'synthetic receipt persistence fault'); END")
        with self.assertRaises(Failure):
            n.call("Secret.Put", put)
        self.assertEqual(self.query("SELECT count(*) FROM versions WHERE kind='Secret'"), [(0,)])
        self.assertEqual(self.query("SELECT count(*) FROM secrets"), [(0,)])
        self.assertEqual(self.query("SELECT count(*) FROM secret_mutations"), [(0,)])
        self.query("DROP TRIGGER fail_receipt")
        before = n.call("Secret.Put", put)
        n.stop()

        row, = self.query("SELECT receipt_tag FROM secret_mutations")
        original, = row
        def replace_tag(tag):
            with closing(sqlite3.connect(self.database)) as db, db:
                db.execute("UPDATE secret_mutations SET receipt_tag=?", (tag,))

        def verify():
            return subprocess.run([*n.daemon, "verify", "--scratch-dir", self.scratch],
                                  text=True, capture_output=True, timeout=15)

        self.assertEqual(verify().returncode, 0)
        replace_tag(bytes([original[0] ^ 1]) + original[1:])
        n.start()
        self.assert_denied("RecoveryRequired", "Secret.Put", put)
        self.assertEqual(n.get(before["resource_id"])["reference"], before)
        n.stop()
        report = verify()
        self.assertNotEqual(report.returncode, 0)
        self.assertIn("Secret mutation receipt is invalid", report.stderr)
        destination = n.base / "unpublished"
        result = subprocess.run([*n.daemon, "checkpoint", "--output", destination],
                                text=True, capture_output=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(destination.exists())
        self.assertNotIn(put["value"], report.stdout + report.stderr + result.stdout + result.stderr)
        replace_tag(original)
        self.assertEqual(verify().returncode, 0)
        n.start()
        self.assertEqual(n.call("Secret.Put", put), before)
