"""Preserve a pending format 25 approval and require the elapsed-deadline writer."""
import argparse
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import sqlite3
import subprocess

from support import Node, target


def exercise(previous, current):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "legacy-approval-clock"})
        deadline = (datetime.now(timezone.utc) + timedelta(minutes=3)).strftime("%Y-%m-%dT%H:%M:%SZ")
        request = node.call("Task.Prepare", {"tool": "volume.write", "key": "old-write",
            "input": dict(**target(volume), name="prev", content="approval survived"),
            "delegation_id": "owner-delegation", "deadline_at": deadline})
        old = node.call("Task.Submit", request)["task_id"]
        task = node.call("Task.Get", {"id": old})
        assert task["lifecycle"] == "WaitingApproval", task
        approval = node.get(task["conditions"][0]["message"])
        identity = node.call("Principal.WhoAmI", {})["authority_id"]
        node.stop()

        binary = current.resolve() / "psiosd"
        verify = [binary, "--config", node.config, "verify"]
        before = json.loads(subprocess.check_output(verify, timeout=15))
        assert (before["node_id"], before["database_format"], before["migration_required"]) == (identity, 25, True), before
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Task.Get", {"id": old}) == task
        assert node.get(approval["reference"]["resource_id"]) == approval
        node.approve(old)
        finished = node.wait(old)
        assert finished["lifecycle"] == "Succeeded", finished
        assert (node.state / "volumes" / volume["resource_id"] / "data" / "prev").read_text() == "approval survived"
        fresh = node.submit("system.status", {})
        assert node.wait(fresh)["lifecycle"] == "Succeeded"
        node.stop()

        after = json.loads(subprocess.check_output(verify, timeout=15))
        assert (after["node_id"], after["database_format"], after["migration_required"]) == (identity, 26, False), after
        with sqlite3.connect(f"file:{node.state / 'state.db'}?mode=ro", uri=True) as db:
            tasks = db.execute("SELECT resource FROM authorization_deadlines JOIN current ON id=resource WHERE kind='Task'").fetchall()
            permits = db.execute("SELECT count(*) FROM authorization_deadlines JOIN current ON id=resource WHERE kind='ExecutionPermit'").fetchone()
        assert tasks == [(fresh,)] and permits == (2,), (tasks, permits)
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS resumed format 25 approval, new Task and permit deadlines, older writer fenced")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
