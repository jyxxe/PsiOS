"""Migrate an undispatched WorkflowRun and fence the former writer."""
import argparse
import json
from pathlib import Path
import sqlite3
import subprocess

from support import Node
from test_schedule import timestamp
from workflow_conditions_upgrade import wait_workflow


def scheduling(node, mode):
    version = node.call("Node.Get", {})["reference"]["version"]
    node.call("Node.Configure", {"expected_resource_version": version, "scheduling": mode})


def exercise(previous, current):
    node = Node(previous)
    try:
        scheduling(node, "Cordoned")
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "ttl_seconds": 120, "units": 2,
            "actions": ["workflow.manage", "system.status", "task.submit", "task.read"], "tool_targets": ["profile-local"]})
        definition = node.call("Workflow.Define", {"key": "upgrade-definition", "steps": [
            {"id": "observe", "tool": "system.status", "input": {}, "after": []}]})
        start = {"key": "previous-run", "definition_ref": definition["reference"],
                 "delegation_id": grant["delegation_ref"]["resource_id"], "deadline_at": timestamp(90)}
        ready = node.call("Workflow.Start", start)
        original = ready["reference"]["resource_id"]
        assert ready["state"]["lifecycle"] == "Ready", ready
        node.stop()

        binary = current.resolve() / "psiosd"
        verify = [binary, "--config", node.config, "verify"]
        before = json.loads(subprocess.check_output(verify, timeout=15))
        assert before["database_format"] == 26 and before["migration_required"], before
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Workflow.Get", {"id": original}) == ready
        assert node.call("Workflow.Start", start) == ready
        fresh = node.call("Workflow.Start", dict(start, key="new-run"))["reference"]["resource_id"]
        scheduling(node, "Enabled")
        for run in (original, fresh):
            finished = wait_workflow(node, run, "Succeeded")
            assert finished["state"]["steps"][0]["lifecycle"] == "Succeeded", finished
        node.stop()

        after = json.loads(subprocess.check_output(verify, timeout=15))
        assert after["node_id"] == before["node_id"] and after["database_format"] >= 27 and not after["migration_required"], after
        with sqlite3.connect(f"file:{node.state / 'state.db'}?mode=ro", uri=True) as db:
            deadlines = db.execute("SELECT resource FROM authorization_deadlines JOIN current ON id=resource WHERE kind='WorkflowRun'").fetchall()
        assert deadlines == [(fresh,)], deadlines
        rejected = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                  capture_output=True, text=True, timeout=15)
        assert rejected.returncode and "SchemaVersionUnsupported" in rejected.stderr, rejected
        print("PASS preserved ready format 26 WorkflowRun, elapsed anchors on new runs, older writer fenced")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
