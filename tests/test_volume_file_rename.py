"""Witness no-replacement Linux renames and recover after a lost committed receipt."""
import hashlib
import json
import os
import signal
import sqlite3
import unittest

from support import Failure, Node, target


class FileRenameTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volume = self.n.perform("volume.create", {"name": "renamed-file"})
        self.root = self.n.state / "volumes" / self.volume["resource_id"] / "data"
        (self.root / "from").mkdir()
        (self.root / "to").mkdir()
        self.source = self.root / "from" / "item"
        self.destination = self.root / "to" / "renamed"
        self.payload = b"\x00volume bytes\xff"
        self.source.write_bytes(self.payload)
        self.grant = self.issue(["volume.verify_file", "volume.rename_file"], self.volume)

    def issue(self, tools, resource):
        return self.n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", *tools],
            "units": 12, "ttl_seconds": 180, "tool_targets": [resource["resource_id"]],
        })

    def submit(self, tool, params, grant=None):
        scope = (grant or self.grant)["delegation_ref"]["resource_id"]
        request = self.n.prepare(tool, params, delegation=scope)
        return self.n.call("Task.Submit", request)["task_id"]

    def artifact(self, task):
        state = self.n.wait(task)
        self.assertEqual(state["lifecycle"], "Succeeded", state)
        ref, = state["result"]["artifact_refs"]
        return self.n.call("Artifact.Read", {"reference": ref})

    def observed(self):
        return self.artifact(self.submit("volume.verify_file", {"target": target(self.volume), "path": "from/item"}))

    def request(self, proof, **options):
        return {"target": target(self.volume), "source": "from/item", "destination": "to/renamed",
                "expected_observation_digest": proof["observation_digest"], **options}

    def scoped_failure(self, code, method, params):
        with self.assertRaises(Failure) as e:
            self.n.call(method, params)
        self.assertEqual(e.exception.code, code, e.exception)

    def test_approval_rechecks_source_and_preserves_destination_before_atomic_rename(self):
        n = self.n
        first = self.observed()
        params = self.request(first)
        with self.assertRaises(Failure) as ungranted:
            n.prepare("volume.rename_file", params)
        self.assertEqual(ungranted.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as escaped:
            self.submit("volume.rename_file", self.request(first, destination="../outside"))
        self.assertEqual(escaped.exception.code, "InvalidArgument")

        stale = self.submit("volume.rename_file", params)
        self.assertEqual(n.wait(stale)["lifecycle"], "WaitingApproval")
        self.assertFalse(self.destination.exists())
        self.source.write_bytes(b"changed since observation")
        n.approve(stale)
        refused = n.wait(stale)
        self.assertEqual(refused["conditions"][0]["type"], "PreconditionFailed")
        self.assertTrue(self.source.exists())
        self.assertFalse(self.destination.exists())

        fresh = self.observed()
        self.destination.write_bytes(b"already here")
        conflict = self.submit("volume.rename_file", self.request(fresh))
        n.approve(conflict)
        self.assertEqual(n.wait(conflict)["conditions"][0]["type"], "Conflict")
        self.assertEqual(self.destination.read_bytes(), b"already here")
        self.assertTrue(self.source.exists())
        self.destination.unlink()
        inode = self.source.stat().st_ino
        moved = self.submit("volume.rename_file", self.request(fresh))
        self.assertEqual(n.wait(moved)["lifecycle"], "WaitingApproval")
        self.assertTrue(self.source.exists())
        n.approve(moved)
        final = self.artifact(moved)
        self.assertFalse(self.source.exists())
        self.assertEqual(self.destination.read_bytes(), b"changed since observation")
        self.assertEqual(self.destination.stat().st_ino, inode)
        self.assertNotEqual(final["volume_ref"]["version"], self.volume["version"])
        self.assertEqual(final["receipt"]["content_digest"], fresh["content_digest"])

    def test_saturated_queue_reconciles_same_inode_after_receipt_loss_and_rejects_stale_proof(self):
        n = self.n
        initial = self.observed()
        inode = self.source.stat().st_ino
        n.stop()
        settings = json.loads(n.config.read_text())
        n.config.write_text(json.dumps(dict(settings, queue_limit=1, recovery_queue_limit=1)))
        n.start()
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute(f"""CREATE TRIGGER lose_rename_receipt BEFORE INSERT ON versions
                 WHEN NEW.id='{self.volume["resource_id"]}' AND NEW.rev>1
                 BEGIN SELECT RAISE(ABORT, 'injected file rename receipt loss'); END""")
        task = self.submit("volume.rename_file", self.request(initial))
        n.approve(task)
        unresolved = n.wait(task)
        self.assertEqual((unresolved["lifecycle"], unresolved["outcome_knowledge"]), ("Paused", "Unknown"), unresolved)
        self.assertEqual(self.destination.stat().st_ino, inode)
        self.assertFalse(self.source.exists())
        op, = unresolved["operation_refs"]
        n.stop(signal.SIGKILL)
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER lose_rename_receipt")
        n.start()
        tool = "volume.inspect_rename"
        grant = self.issue([tool], op)

        def inspect():
            inspected = n.wait(self.submit(tool, target(op), grant))
            self.assertEqual(inspected["lifecycle"], "Succeeded", inspected)
            return inspected["result"]["artifact_refs"][0]

        first = inspect()
        details = n.call("Artifact.Read", {"reference": first})
        self.assertEqual(details["placement"], "Renamed")
        self.assertEqual(details["content_digest"], initial["content_digest"])
        old = self.destination.stat()
        self.destination.write_bytes(b"X" * len(self.payload))
        os.utime(self.destination, ns=(old.st_atime_ns, old.st_mtime_ns))
        self.scoped_failure("OutcomeUnknown", "Operation.Reconcile", {**target(op), "evidence_ref": first})
        self.assertEqual(n.wait(task), unresolved)
        mismatched = n.wait(self.submit(tool, target(op), grant))
        self.assertEqual(mismatched["conditions"][0]["type"], "PreconditionFailed")
        self.destination.write_bytes(self.payload)
        os.utime(self.destination, ns=(old.st_atime_ns, old.st_mtime_ns))
        fixed = n.call("Operation.Reconcile", {**target(op), "evidence_ref": inspect()})
        self.assertEqual(fixed["outcome_knowledge"], "Known")
        result = self.artifact(task)
        self.assertEqual(result["receipt"]["content_digest"], "sha256:" + hashlib.sha256(self.payload).hexdigest())
        self.assertEqual(self.destination.stat().st_ino, inode)
        self.assertFalse(self.source.exists())
        self.assertEqual(n.call("Node.Get", {})["admission"]["ordinary"]["used"], 0)
