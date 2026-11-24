"""Upgrade an active format 27 timer while preserving its trigger and UTC cutoff."""
import argparse
import json
from pathlib import Path
import sqlite3
import subprocess

from schedule_repeat_upgrade import settled
from support import Node
from test_schedule import timestamp
from workflow_deadline_upgrade import scheduling


def exercise(previous, current):
    node = Node(previous)
    try:
        scheduling(node, "Cordoned")
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 2, "ttl_seconds": 120,
            "actions": ["schedule.manage", "system.status", "task.submit", "task.read"], "tool_targets": ["profile-local"]})
        spec = {"key": "legacy-cutoff", "tool": "system.status", "input": {}, "run_at": timestamp(1.5),
                "deadline_at": timestamp(60), "delegation_id": grant["delegation_ref"]["resource_id"]}
        legacy = node.call("Schedule.Create", spec)
        node.stop()

        binary = current.resolve() / "psiosd"
        verify = [binary, "--config", node.config, "verify"]
        before = json.loads(subprocess.check_output(verify, timeout=15))
        assert before["database_format"] == 27 and before["migration_required"], before
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Schedule.Create", spec) == legacy
        fresh = node.call("Schedule.Create", dict(spec, key="new-cutoff", run_at=timestamp(1)))
        scheduling(node, "Enabled")
        for initial in (legacy, fresh):
            fired = settled(node, initial)
            assert fired["lifecycle"] == "Triggered" and fired["trigger_id"] == initial["state"]["trigger_id"], fired
            assert node.wait(fired["task_receipt"]["task_id"])["lifecycle"] == "Succeeded"
        node.stop()

        after = json.loads(subprocess.check_output(verify, timeout=15))
        assert after["node_id"] == before["node_id"] and after["database_format"] >= 28 and not after["migration_required"], after
        with sqlite3.connect(f"file:{node.state / 'state.db'}?mode=ro", uri=True) as db:
            deadlines = db.execute("SELECT resource FROM authorization_deadlines JOIN current ON id=resource WHERE kind='Schedule'").fetchall()
        assert deadlines == [(fresh["reference"]["resource_id"],)], deadlines
        rejected = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                  capture_output=True, text=True, timeout=15)
        assert rejected.returncode and "SchemaVersionUnsupported" in rejected.stderr, rejected
        print("PASS preserved format 27 timer, next-generation schedule cutoff, and older writer fencing")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
