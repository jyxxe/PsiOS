"""Resume a revision-ordered snapshot and its watch after installing origin indexes."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, target


def main(previous):
    node = Node(previous)
    try:
        ref = node.call("Node.Get", {})["reference"]
        node.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": "Cordoned"})
        first, second, third = [node.submit("system.status", {}) for _ in range(3)]
        node.call("Task.Cancel", target(node.get(first)["reference"]))
        first_at_snapshot, second_at_snapshot, third_at_snapshot = [
            node.get(task)["reference"] for task in (first, second, third)
        ]
        page = node.call("Resource.List", {"kind": "Task", "limit": 1})
        assert page["items"] == [second_at_snapshot] and page["next_cursor"], page
        node.call("Task.Cancel", target(third_at_snapshot))
        current_third = node.get(third)["reference"]
        fourth = node.get(node.submit("system.status", {}))["reference"]
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 51 and before["migration_required"], before
        node.start()
        remaining = node.call("Resource.List", {"cursor": page["next_cursor"], "limit": 2})
        assert remaining["complete"] and remaining["items"] == [third_at_snapshot, first_at_snapshot], remaining
        assert remaining["snapshot_id"] == page["snapshot_id"], remaining
        changes = node.call("Resource.Watch", {"cursor": remaining["watch_start_cursor"]})
        assert [event["resource_ref"] for event in changes["events"]] == [current_third, fourth], changes

        new_page = node.call("Resource.List", {"kind": "Task", "limit": 2})
        assert new_page["items"] == [first_at_snapshot, second_at_snapshot] and new_page["next_cursor"], new_page
        tail = node.call("Resource.List", {"cursor": new_page["next_cursor"]})
        assert tail["complete"] and tail["items"] == [current_third, fourth], tail
        assert tail["snapshot_id"] == new_page["snapshot_id"], tail
        assert not node.call("Resource.Watch", {"cursor": tail["watch_start_cursor"]})["events"]
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 52 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 51 snapshot continued in original order and its watch delivered subsequent revisions")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
