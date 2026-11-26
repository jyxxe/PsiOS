"""Recover actual volume writes after the publication syscall boundary."""
import base64
from contextlib import closing
import json
import os
import signal
import sqlite3
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target


@unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_WRITE_CUT") == "1",
                     "requires strace filesystem fault injection")
class DurableVolumeWriteReconciliation(unittest.TestCase):
    def start_traced(self, node, path, fault, trace):
        command = node.daemon
        node.daemon = ["strace", "-D", "-f", "-yy", "-o", str(trace), "-P", str(path),
                       "-e", "trace=fsync", "-e", f"inject=fsync:{fault}:when=1", "--", *command]
        try:
            node.start()
        finally:
            node.daemon = command
        self.assertTrue(os.path.samefile(f"/proc/{node.process.pid}/exe", command[0]))

    def test_write_and_append_keep_the_hold_until_file_and_parent_are_durable(self):
        for tool in ("volume.write", "volume.append"):
            with self.subTest(tool=tool):
                node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
                try:
                    self.check_publication(node, tool)
                finally:
                    node.close()

    def test_cancel_after_killed_write_keeps_original_effect_until_reconciled(self):
        for by_origin in (False, True):
            with self.subTest(by_origin=by_origin):
                node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
                try:
                    self.check_cancelled_write(node, by_origin)
                finally:
                    node.close()

    def check_cancelled_write(self, node, by_origin):
        volume = node.perform("volume.create", {"name": "cancel-after-lost-receipt"})
        payload = node.state / "volumes" / volume["resource_id"] / "data" / "record"
        node.stop()
        self.start_traced(node, payload.parent, "error=EIO:signal=SIGKILL", node.base / "cancel.trace")
        request = node.prepare("volume.write", {**target(volume), "name": payload.name, "content": "published"})
        receipt = node.call("Task.Submit", request)
        task_id = receipt["task_id"]
        self.assertEqual(node.wait(task_id)["lifecycle"], "WaitingApproval")
        try:
            node.approve(task_id)
        except (ConnectionError, struct.error):
            pass  # The process can die before it acknowledges the approval.
        self.assertEqual(node.process.wait(20), -signal.SIGKILL)
        self.assertEqual(payload.read_bytes(), b"published")
        inode = payload.stat()

        node.start()
        pending = node.wait(task_id)
        self.assertEqual((pending["lifecycle"], pending["outcome_knowledge"]), ("Paused", "Unknown"))
        [operation] = pending["operation_refs"]
        run_id = pending["current_run_id"]
        op_before = node.get(operation["resource_id"])
        run_before = node.get(run_id)

        def custody():
            with closing(sqlite3.connect(node.state / "state.db")) as database:
                return database.execute(
                    "SELECT q.phase,q.operation,h.holder,b.reserved,b.spent FROM queue q "
                    "JOIN holds h ON h.holder=q.operation AND h.volume=? "
                    "JOIN budgets b ON b.id=q.budget WHERE q.task=?",
                    (volume["resource_id"], task_id),
                ).fetchone()

        held = custody()
        self.assertIsNotNone(held)
        self.assertEqual(held[:3], ("Unknown", operation["resource_id"], operation["resource_id"]))
        old_target = {"id": task_id, "expected_resource_version": pending["task_revision"]}
        origin_cancel = {name: request[name] for name in (
            "schema_version", "origin_ref", "idempotency_key", "request_digest", "cancel_scope_id",
        )}
        origin_cancel.update(cancel_request_id=key(), reason="Cancel after publication receipt was lost")
        if by_origin:
            with self.assertRaises(Failure) as mismatch:
                node.call("Task.CancelOrigin", dict(origin_cancel, request_digest="sha256:" + "0" * 64))
            self.assertEqual(mismatch.exception.code, "IdempotencyConflict")
            self.assertEqual(node.wait(task_id), pending)
            cancelled = node.call("Task.CancelOrigin", origin_cancel)
        else:
            cancelled = node.call("Task.Cancel", old_target)

        current_target = dict(old_target, expected_resource_version=cancelled["task_revision"])
        self.assertNotEqual(current_target, old_target)
        self.assertEqual(cancelled, dict(pending, lifecycle="Cancelled", task_revision=cancelled["task_revision"]))
        self.assertEqual(node.get(operation["resource_id"]), op_before)
        self.assertEqual(node.get(run_id), run_before)
        self.assertEqual(node.get(volume["resource_id"])["reference"], volume)
        self.assertEqual(custody(), held)
        with self.assertRaises(Failure) as stale:
            node.call("Task.Cancel", old_target)
        self.assertEqual(stale.exception.code, "Conflict")
        self.assertEqual(node.call("Task.Cancel", current_target), cancelled)
        self.assertEqual(node.call("Task.Wait", {"id": task_id, "timeout_ms": 1000,
                                               "after_version": pending["task_revision"]}), cancelled)
        for method, extra, code in (
            ("Task.Retry", {"key": key()}, "OutcomeUnknown"),
            ("Task.Archive", {}, "PreconditionFailed"),
            ("Task.EvictResults", {"reason": "must retain unresolved custody"}, "PreconditionFailed"),
        ):
            with self.assertRaises(Failure) as refused:
                node.call(method, {**current_target, **extra})
            self.assertEqual(refused.exception.code, code, refused.exception)

        node.stop(signal.SIGKILL)
        verified = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        self.assertEqual(verified["sqlite_integrity"], "Verified", verified)
        node.start()
        self.assertEqual(node.wait(task_id), cancelled)
        self.assertEqual(custody(), held)
        if by_origin:
            self.assertEqual(node.call("Task.CancelOrigin", origin_cancel), cancelled)
        replay = node.call("Task.Submit", request)
        self.assertTrue(replay["deduplicated"])
        self.assertEqual(replay["task_id"], task_id)

        settled = node.call("Operation.Reconcile", target(operation))
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Cancelled", "Known"))
        self.assertEqual(settled["effect_refs"][0]["resource_id"],
                         op_before["value"]["effect_refs"][0]["resource_id"])
        done = node.wait(task_id)
        self.assertEqual((done["lifecycle"], done["outcome_knowledge"], done["current_run_id"]),
                         ("Cancelled", "Known", run_id))
        self.assertIsNone(custody())
        with closing(sqlite3.connect(node.state / "state.db")) as database:
            self.assertEqual(database.execute(
                "SELECT q.phase,b.reserved,b.spent FROM queue q JOIN budgets b ON b.id=q.budget WHERE q.task=?",
                (task_id,),
            ).fetchone(), ("Done", *held[3:]))
        [result] = done["result"]["artifact_refs"]
        proof = node.get(result["resource_id"])["resource"]["spec"]["reconciliation_evidence_ref"]
        recorded = node.get(proof["resource_id"])["value"]
        self.assertEqual(recorded["operation_ref"], operation)
        self.assertEqual(recorded["evidence"]["file_revision"]["inode"], str(inode.st_ino))
        self.assertEqual(recorded["evidence"]["conclusion"], "ObservedPostcondition")
        self.assertEqual(payload.read_bytes(), b"published")
        self.assertEqual((payload.stat().st_ino, payload.stat().st_mtime_ns), (inode.st_ino, inode.st_mtime_ns))
        with self.assertRaises(Failure) as duplicate:
            node.call("Operation.Reconcile", target(operation))
        self.assertEqual(duplicate.exception.code, "Conflict")
        self.assertEqual(node.wait(task_id), done)
        if by_origin:
            self.assertEqual(node.call("Task.CancelOrigin", origin_cancel), done)
        node.stop()
        verified = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        self.assertEqual(verified["sqlite_integrity"], "Verified", verified)

    def check_publication(self, node, tool):
        volume = node.perform("volume.create", {"name": tool})
        directory = node.state / "volumes" / volume["resource_id"] / "data"
        if tool == "volume.append":
            node.perform("volume.mkdir", {"target": target(volume), "path": "nested"})
            volume = node.get(volume["resource_id"])["reference"]
            node.perform(tool, {"target": target(volume), "path": "nested/payload", "offset": 0,
                                "data_base64": base64.b64encode(b"prefix:").decode("ascii")})
            volume = node.get(volume["resource_id"])["reference"]
            file = directory / "nested" / "payload"
            params = {"target": target(volume), "path": "nested/payload", "offset": 7,
                      "data_base64": base64.b64encode(b"new").decode("ascii")}
            expected = b"prefix:new"
        else:
            file = directory / "record"
            params = {**target(volume), "name": file.name, "content": "new"}
            expected = b"new"

        node.stop()
        original_trace = node.base / (tool + ".publish")
        self.start_traced(node, file.parent, "error=EIO:signal=SIGKILL", original_trace)
        job = node.submit(tool, params)
        before = node.wait(job)
        self.assertEqual(before["lifecycle"], "WaitingApproval")
        try:
            node.approve(job)
        except (ConnectionError, struct.error):
            pass  # Approval may commit before the interrupted client reply.
        self.assertEqual(node.process.wait(20), -signal.SIGKILL)
        self.assertEqual(file.read_bytes(), expected)
        deadline = time.monotonic() + 3
        while True:
            lines = [line for line in original_trace.read_text().splitlines()
                     if "fsync(" in line and f"<{file.parent}>" in line]
            if lines:
                break
            self.assertLess(time.monotonic(), deadline, "trace did not reach the parent sync")
            time.sleep(0.01)
        self.assertFalse(any("= 0" in line for line in lines), lines)

        # Lose each sync independently. Neither failure may record a receipt
        # or release the durable hold, even though the exact bytes can be read.
        for index, blocked_path in enumerate((file, file.parent)):
            retry_trace = node.base / f"{tool}.retry{index}"
            self.start_traced(node, blocked_path, "error=EIO", retry_trace)
            pending = node.wait(job)
            self.assertEqual((pending["lifecycle"], pending["outcome_knowledge"]), ("Paused", "Unknown"))
            [operation] = pending["operation_refs"]
            with self.assertRaises(Failure) as failed:
                node.call("Operation.Reconcile", target(operation))
            self.assertEqual(failed.exception.code, "OutcomeUnknown", failed.exception)
            self.assertEqual(node.wait(job), pending)
            self.assertEqual(node.get(volume["resource_id"])["reference"], volume)
            with closing(sqlite3.connect(node.state / "state.db")) as database:
                hold = database.execute("SELECT holder FROM holds WHERE volume=?",
                                        (volume["resource_id"],)).fetchone()
            self.assertEqual(hold, (operation["resource_id"],))
            node.stop()
            self.assertIn(f"<{blocked_path}>) = -1 EIO", retry_trace.read_text())

        identity = file.stat()
        node.start()
        self.assertEqual(node.call("Operation.Reconcile", target(operation))["outcome_knowledge"], "Known")
        completed = node.wait(job)
        self.assertEqual((completed["lifecycle"], completed["current_run_id"]), ("Succeeded", pending["current_run_id"]))
        self.assertEqual(file.read_bytes(), expected)
        self.assertEqual((file.stat().st_ino, file.stat().st_mtime_ns), (identity.st_ino, identity.st_mtime_ns))
        [artifact] = completed["result"]["artifact_refs"]
        proof = node.get(artifact["resource_id"])["resource"]["spec"]["reconciliation_evidence_ref"]
        evidence = node.get(proof["resource_id"])["value"]["evidence"]
        self.assertEqual(evidence["file_revision"]["inode"], str(identity.st_ino))
        self.assertEqual(evidence["conclusion"], "ObservedPostcondition")
