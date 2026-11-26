"""Verify writable predecessors cannot read the private review Signal format."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import unittest

from test_work_item_review_signals import WorkItemReviewSignalFixture


class WorkItemReviewSignalsUpgrade(WorkItemReviewSignalFixture, unittest.TestCase):
    def test_previous_mailbox_and_work_item_gain_private_reply_after_upgrade(self):
        n = self.n
        self.result_grant()
        request = self.request("typed-after-upgrade")
        self.denied("PermissionDenied", "Signal.Send", request, self.sender)
        ordinary = self.call("Signal.Send", {
            "mailbox_id": self.mailbox, "key": "legacy-notice", "topic": "ReviewRequested",
        }, self.sender)
        legacy = n.daemon[:]
        n.stop(signal.SIGKILL)
        source = json.loads(subprocess.check_output(legacy + ["verify"], timeout=20))
        self.assertIn(source["database_format"], (107, 108, 109))
        n.daemon[0] = str(Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).absolute() / "psiosd")
        pending = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertTrue(pending["migration_required"])
        n.start()
        sent = self.call("Signal.Send", request, self.sender)
        first, second = self.inbox()["items"]
        self.assertEqual((first["reference"], first["signal"]["seq"]), (ordinary, 1))
        self.assertEqual((second["reference"], second["signal"]["seq"]), (sent, 2))
        self.assertEqual(second["signal"]["resource_ref"], self.item)
        reply = self.call("WorkItem.Review", self.reply(sent), self.reviewer)
        n.stop(signal.SIGKILL)
        rejected = subprocess.run(legacy + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(rejected.returncode, 0, rejected)
        self.assertIn("SchemaVersionUnsupported", rejected.stderr)
        upgraded = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertGreaterEqual(upgraded["database_format"], 110)
        self.assertEqual(upgraded["node_id"], source["node_id"])
        self.assertFalse(upgraded["migration_required"])
        n.start()
        self.assertEqual(self.receipt(sent, self.sender), reply)
        self.assertEqual(self.call("WorkItem.Review", self.reply(sent), self.reviewer), reply)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True)
    WorkItemReviewSignalsUpgrade.bin_dir = parser.parse_args().previous_bin_dir
    unittest.main(argv=[__file__])
