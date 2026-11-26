"""Run live, unsnapshotted default leases from a verified previous Core."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, target
from test_agent_tool_context import AgentToolContextFixture


class DefaultContextUpgrade(AgentToolContextFixture, unittest.TestCase):
    def change_task_revision(self, task, reference):
        n = self.node
        old_ref = n.get(task)["reference"]
        old_result = n.call("Task.Get", {"id": task})
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn:
            artifact = (reference["resource_id"],)
            payload, = conn.execute("SELECT payload FROM artifact_chunks WHERE artifact_id=? AND chunk_index=0",
                                    artifact).fetchone()
            with conn:
                conn.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload)) "
                             "WHERE artifact_id=? AND chunk_index=0", artifact)
            try:
                unavailable = n.call("Task.VerifyResults", target(old_ref))
                self.assertEqual(unavailable["result"]["availability"], "Unavailable")
                invalid_ref = n.get(task)["reference"]
            finally:
                with conn:
                    conn.execute("UPDATE artifact_chunks SET payload=? WHERE artifact_id=? AND chunk_index=0",
                                 (payload, *artifact))
        repaired = n.call("Task.VerifyResults", target(invalid_ref))
        current_ref = n.get(task)["reference"]
        self.assertEqual(repaired, old_result)
        self.assertEqual(current_ref["digest"], old_ref["digest"])
        self.assertNotEqual(current_ref["version"], old_ref["version"])

    def upgrade_in_flight(self, *, changed):
        n = self.node
        token = self.agent["token"]
        session = self.open(context_policy="ReferencesOnly")
        task, result = self.observe(self.claim("previous-first"), "system.status")
        issued = self.claim("previous-pending")
        self.assertEqual(issued["context"]["last_tool"]["result"]["artifact_refs"], [result])
        if changed:
            self.change_task_revision(task, result)
        legacy = n.daemon[:]
        n.stop(signal.SIGKILL)
        source = json.loads(subprocess.check_output(legacy + ["verify"], timeout=20))
        self.assertEqual(source["database_format"], 106)
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            self.assertIsNone(conn.execute("SELECT lease FROM model_context_snapshots WHERE lease=?",
                                           (issued["lease_id"],)).fetchone())
        n.daemon[0] = str(Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).absolute() / "psiosd")
        before = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertTrue(before["migration_required"])
        n.start()
        current = n.call("Session.Get", {"id": session["resource_id"]}, token)
        if changed:
            with self.assertRaises(Failure) as unknown:
                self.claim("previous-pending")
            self.assertEqual(unknown.exception.code, "OutcomeUnknown", unknown.exception)
            self.assertEqual(self.claim("no-replacement"), {"available": False})
        else:
            self.assertEqual(self.claim("previous-pending"), {**issued, "recovered": True})
        self.assertEqual(n.call("Session.Get", {"id": session["resource_id"]}, token), current)
        self.assertEqual(self.budget(), 2)

        # An already delivered exact lease can still settle a retained response.
        receipt = self.decision(issued, {"action": "finish", "summary": "Original request settled."})
        self.assertEqual(receipt["lifecycle"], "Succeeded")
        self.assertEqual(self.claim("previous-pending")["completed"], receipt)
        self.assertEqual(self.budget(), 2)
        history = n.call("Session.History", {"id": session["resource_id"]}, token)
        self.assertEqual(len(history["tasks"]), 1)
        n.stop()
        final = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertGreaterEqual(final["database_format"], 107)
        self.assertEqual(final["node_id"], source["node_id"])
        self.assertFalse(final["migration_required"])
        obsolete = subprocess.run(legacy + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(obsolete.returncode, 0, obsolete)
        self.assertIn("SchemaVersionUnsupported", obsolete.stderr)

    def test_original_task_revision_replays_without_charging(self):
        self.upgrade_in_flight(changed=False)

    def test_same_result_after_task_revision_change_has_unknown_replay(self):
        self.upgrade_in_flight(changed=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True)
    DefaultContextUpgrade.bin_dir = parser.parse_args().previous_bin_dir
    unittest.main(argv=[__file__])
