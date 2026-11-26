"""Read and advance a checked checkpoint with the previous same-format writer."""

import argparse
import json
from pathlib import Path
import signal
import subprocess
import unittest

from test_event_offset_generation_ack import EventOffsetGenerationAck


class EventOffsetGenerationCompatibility(EventOffsetGenerationAck):
    def verify(self):
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertEqual(report["sqlite_integrity"], "Verified", report)
        self.assertFalse(report["migration_required"], report)
        return report["database_format"], report["node_id"]

    def test_same_format_writer_preserves_and_can_supersede_checked_revision(self):
        node, consumer = self.node, "same-format-writer"
        original = self.create(consumer, self.cursor())
        checked = self.checked(consumer, original, self.cursor())
        saved = node.call("Event.Ack", checked)
        self.assertEqual(saved, self.offset(consumer))

        node.stop(signal.SIGKILL)
        current, identity = node.daemon[0], self.verify()
        node.daemon[0] = str(self.previous_bin_dir / "psiosd")
        try:
            self.assertEqual(self.verify(), identity)
            node.start()
            self.assertEqual(self.offset(consumer), saved)
            self.reject("InvalidArgument", checked)
            cursor = self.cursor()
            self.assertEqual(node.call("Event.Ack", {"consumer": consumer, "previous": saved["cursor"],
                                                     "cursor": cursor}), {"cursor": cursor})
            legacy = self.offset(consumer)
            self.assertNotEqual(legacy["revision"], saved["revision"])
        finally:
            try:
                node.stop(signal.SIGKILL)
            finally:
                node.daemon[0] = current

        self.assertEqual(self.verify(), identity)
        node.start()
        self.assertEqual(self.offset(consumer), legacy)
        self.reject("Conflict", checked)
        cursor = self.cursor()
        advanced = node.call("Event.Ack", self.checked(consumer, legacy, cursor))
        self.assertEqual(advanced, self.offset(consumer))
        self.assertEqual(advanced["cursor"], cursor)
        node.stop()
        self.assertEqual(self.verify(), identity)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True, type=Path)
    EventOffsetGenerationCompatibility.previous_bin_dir = parser.parse_args().previous_bin_dir.resolve()
    unittest.main(argv=[__file__, "EventOffsetGenerationCompatibility."
                        "test_same_format_writer_preserves_and_can_supersede_checked_revision"])
