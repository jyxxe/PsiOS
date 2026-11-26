"""Recover actual volume writes after the publication syscall boundary."""
import base64
from contextlib import closing
import os
import signal
import sqlite3
import struct
import time
import unittest

from support import Failure, Node, target


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
