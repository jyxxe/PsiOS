"""Continue an acknowledged, resource-scoped cursor from format 43 after upgrading."""
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
        task = node.submit("system.status", {})
        unrelated = node.submit("system.status", {})
        initial = node.call("Resource.List", {"kind": "Task", "id": task})
        original = node.get(task)["reference"]
        assert initial["items"] == [original], initial
        saved = node.call("Event.Ack", {"consumer": "format-43-watch", "cursor": initial["watch_start_cursor"]})
        node.call("Task.Cancel", target(node.get(unrelated)["reference"]))
        node.call("Task.Cancel", target(original))
        expected = node.get(task)["reference"]
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 43 and before["migration_required"], before
        node.start()
        assert node.call("Event.Offset", {"id": "format-43-watch"}) == saved
        changes = node.call("Resource.Watch", saved)
        assert [event["resource_ref"] for event in changes["events"]] == [expected], changes
        node.call("Event.Ack", {"consumer": "format-43-watch", "previous": saved["cursor"], "cursor": changes["cursor"]})
        assert not node.call("Event.Poll", {"cursor": changes["cursor"]})["events"]
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 44 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 43 scoped cursor delivered only the original committed change after upgrade")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
