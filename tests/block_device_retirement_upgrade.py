"""Keep genuine format-110 Linux block enrollments across indexed retirement migration."""
import argparse
from contextlib import closing
import json
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Node, target
from test_block_layout import attached, enroll, issue, receipt, scratch_gpt


def exercise(previous, current):
    node = Node(previous)
    try:
        authority = node.call("Node.Get", {})["reference"]["resource_id"]
        owner = "local-owner"
        layout = issue(node, owner, authority)
        tracker = node.call("Grant.Issue", {"principal_id": owner, "ttl_seconds": 120, "units": 8,
            "actions": ["node.track_block_device", "task.submit", "task.read"], "tool_targets": [authority]})
        original, replacement, live = (node.base / name for name in ("old.raw", "replacement.raw", "live.raw"))
        for image in (original, replacement, live):
            scratch_gpt(image)

        with attached(live) as (_, live_sysfs):
            kept = enroll(node, live_sysfs, layout, tracker)["device_ref"]
            with attached(original) as (name, old_sysfs):
                old = enroll(node, old_sysfs, layout, tracker)["device_ref"]
            grant = node.call("Grant.Issue", {"principal_id": owner, "ttl_seconds": 120, "units": 8,
                "actions": ["device.block_layout", "task.submit", "task.read"], "tool_targets": [old["resource_id"]]})
            with attached(replacement, name):
                request = node.prepare("device.block_layout", target(old),
                                       delegation=grant["delegation_ref"]["resource_id"])
                terminal = receipt(node, node.call("Task.Submit", request)["task_id"])["device_ref"]
                assert terminal["version"] == "r2", terminal
            old_state = node.call("BlockDevice.Get", {"id": old["resource_id"]})
            assert old_state["state"]["lifecycle"] == "Replaced", old_state
            kept_before = node.get(kept["resource_id"])
            assert kept_before["value"]["lifecycle"] == "Present", kept_before
            node.stop(signal.SIGKILL)

            legacy = node.daemon[:]
            node.daemon[0] = str(current.resolve() / "psiosd")
            before = json.loads(subprocess.check_output(
                node.daemon + ["verify", "--rehearse-migration"], timeout=20))
            assert before["database_format"] == 110 and before["migration_required"], before
            rehearsal = before["migration_rehearsal"]
            assert rehearsal["source_format"] == 110 and rehearsal["status"] == "Verified", before
            target_format = rehearsal["target_format"]
            assert target_format >= 111, before
            node.start()
            assert node.get(kept["resource_id"]) == kept_before
            assert node.call("BlockDevice.Get", {"id": old["resource_id"]}) == old_state
            with closing(sqlite3.connect(node.state / "state.db")) as db:
                actual = db.execute("SELECT device_id FROM block_device_watch ORDER BY origin_seq").fetchall()
                assert actual == [(kept["resource_id"],)], actual
            retired = node.call("BlockDevice.Retire", target(kept))
            assert retired["state"]["retired_from_ref"] == kept, retired
            node.stop()
            after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
            assert after["database_format"] == target_format and not after["migration_required"], after
            refused = subprocess.run(legacy + ["check"], text=True, capture_output=True, timeout=20)
            assert refused.returncode and "SchemaVersionUnsupported" in refused.stderr, refused
        print(f"PASS format-110 to {target_format} block backfill, terminal history, retirement and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", type=Path, required=True)
    parser.add_argument("--current-bin-dir", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin_dir, args.current_bin_dir)
