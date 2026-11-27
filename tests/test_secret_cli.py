"""Exercise Secret lifecycle commands through a real local authority."""
import base64
from contextlib import closing
import hashlib
import hmac
import json
import os
import sqlite3
import subprocess
import unittest

from support import Node


class SecretCliTests(unittest.TestCase):
    def setUp(self):
        self.node = Node()
        self.addCleanup(self.node.close)
        self.token = self.node.witness / "owner.token"

    def command(self, *args, token=None):
        return [self.node.binaries / "psiosctl", "--socket", self.node.socket,
                "--token-file", token or self.token, "secret", *args]

    def run_cli(self, *args, data=b"", token=None):
        return subprocess.run(self.command(*args, token=token), input=data,
                              capture_output=True, timeout=10)

    def verify_mac(self, reference, secret):
        message = "verify synthetic fixture"
        result = self.node.perform("secret.mac", {"secret_id": reference["resource_id"], "message": message})
        expected = hmac.new(secret, message.encode(), hashlib.sha256).digest()
        self.assertEqual(base64.b64decode(result["tag"], validate=True), expected)

    def test_cli_preserves_exact_input_and_guards_rotation_and_revocation(self):
        first = "  synthetic UTF-8 键\n\0".encode()
        imported = self.run_cli("put", "cli-vault-entry", data=first)
        self.assertEqual(imported.returncode, 0, imported.stderr)
        self.assertNotIn(first, imported.stdout + imported.stderr)
        reference = json.loads(imported.stdout)
        self.assertEqual(set(reference), {"resource_id", "version", "digest"})
        self.verify_mac(reference, first)

        second = b"synthetic next value\n"
        file = self.node.base / "private-value"
        file.touch(mode=0o600)
        file.write_bytes(second)
        rotated = self.run_cli("rotate", reference["resource_id"], "--revision",
                               reference["version"], "--value-file", file)
        self.assertEqual(rotated.returncode, 0, rotated.stderr)
        self.assertNotIn(second, rotated.stdout + rotated.stderr)
        current = json.loads(rotated.stdout)
        self.assertEqual(current["resource_id"], reference["resource_id"])
        self.assertNotEqual(current["version"], reference["version"])
        self.verify_mac(current, second)
        stale = self.run_cli("rotate", reference["resource_id"], "--revision",
                             reference["version"], data=b"stale retry")
        self.assertNotEqual(stale.returncode, 0)
        self.assertIn(b"Conflict", stale.stderr)
        self.verify_mac(current, second)

        revoked = self.run_cli("revoke", current["resource_id"], "--revision", current["version"])
        self.assertEqual(revoked.returncode, 0, revoked.stderr)
        self.assertEqual(self.node.get(current["resource_id"])["value"]["lifecycle"], "Revoked")
        task = self.node.wait(self.node.submit("secret.mac", {
            "secret_id": current["resource_id"], "message": "after revoke"}))
        self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]),
                         ("Failed", "PermissionDenied"))

    def test_cli_refuses_invalid_and_unapproved_input_before_creating_a_secret(self):
        source = self.node.base / "public-value"
        source.write_bytes(b"synthetic rejected input")
        source.chmod(0o644)
        refused = self.run_cli("put", "public-input", "--value-file", source)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(b"private regular secret value file", refused.stderr)
        source.chmod(0o600)
        link = self.node.base / "secret-link"
        link.symlink_to(source)
        pipe = self.node.base / "named-pipe"
        os.mkfifo(pipe, mode=0o600)
        for file in (link, pipe):
            self.assertNotEqual(self.run_cli("put", "nonregular-input", "--value-file", file).returncode, 0)
        for data in (b"", b"\xff", b"a" * 8193):
            self.assertNotEqual(self.run_cli("put", "invalid-input", data=data).returncode, 0)

        master, slave = os.openpty()
        try:
            terminal = subprocess.run(self.command("put", "terminal-input"), stdin=slave,
                                      capture_output=True, timeout=10)
        finally:
            os.close(slave)
            os.close(master)
        self.assertNotEqual(terminal.returncode, 0)
        self.assertIn(b"requires a pipe or a private value file", terminal.stderr)

        agent = self.node.call("Agent.Register", {"name": "without-secret-management"})
        token = self.node.base / "agent.token"
        token.touch(mode=0o600)
        token.write_text(agent["token"])
        denied = self.run_cli("put", "unapproved-input", data=b"synthetic bytes", token=token)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn(b"PermissionDenied", denied.stderr)
        with closing(sqlite3.connect(self.node.state / "state.db")) as database:
            self.assertEqual(database.execute("SELECT count(*) FROM current WHERE kind='Secret'").fetchone(), (0,))
