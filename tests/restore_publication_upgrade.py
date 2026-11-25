"""Restore an existing format 41 backup after registering the recovery capability."""
import argparse
import json
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Node, target


def main(previous):
    node = Node(previous)
    try:
        origin = node.perform("volume.create", {"name": "before-restore-upgrade"})
        content = node.state / "volumes" / origin["resource_id"] / "data" / "content"
        content.write_text("retained format 41 backup")
        backup = node.perform("backup.create", target(origin))
        old_source = node.perform("backup.create", target(origin))
        with sqlite3.connect(node.state / "state.db", timeout=10) as database:
            database.execute("""CREATE TRIGGER lose_old_restore BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.spec->>'storage_restore_phase'='AwaitingActivation'
                BEGIN SELECT RAISE(ABORT, 'format 41 lost restore receipt'); END""")
        old_task = node.submit("backup.restore", target(old_source))
        unknown = node.wait(old_task)
        assert unknown["lifecycle"] == "Paused" and unknown["outcome_knowledge"] == "Unknown", unknown
        old_op = unknown["operation_refs"][0]
        node.stop(signal.SIGKILL)
        with sqlite3.connect(node.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER lose_old_restore")
        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 41 and before["migration_required"], before
        node.start()
        capability = node.get("tool-volume.inspect_publication")["value"]
        assert capability["actions"] == ["volume.inspect_publication"], capability
        assert node.wait(old_task) == unknown
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 1, "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "volume.inspect_publication"],
            "tool_targets": [old_op["resource_id"]],
        })
        inspection = node.prepare("volume.inspect_publication", target(old_op),
                                  delegation=grant["delegation_ref"]["resource_id"])
        absent = node.wait(node.call("Task.Submit", inspection)["task_id"])
        assert absent["conditions"][0]["type"] == "RecoveryRequired", absent
        assert node.wait(old_task) == unknown
        restored = node.perform("backup.restore", target(backup))
        data = node.state / "volumes" / restored["resource_id"] / "data" / "content"
        assert data.read_text() == "retained format 41 backup"
        spec = node.get(restored["resource_id"])["resource"]["spec"]
        assert spec["restored_from"] == backup and spec["storage_restore_phase"] == "AwaitingActivation", spec
        node.perform("volume.activate", target(restored))
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 42 and not after["migration_required"], after
        assert before["node_id"] == after["node_id"]
        print("PASS format 41 preserved an ambiguous restore and activated a new restore of a retained backup")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
