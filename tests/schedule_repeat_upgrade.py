"""Verify that a format-10 timer survives the interval scheduler upgrade."""
import argparse
from pathlib import Path
import json
import signal
import subprocess
import time

from support import Node, key
from test_schedule import after, timestamp


def settled(node, schedule):
    requested = {"id": schedule["reference"]["resource_id"]}
    deadline = time.monotonic() + 10
    while True:
        current = node.call("Schedule.Get", requested)["state"]
        if current["lifecycle"] != "Pending":
            return current
        assert time.monotonic() < deadline, current
        time.sleep(0.05)


def upgrade(previous, current):
    node = Node(previous)
    try:
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "tool_targets": ["profile-local"],
            "actions": ["task.submit", "task.read", "schedule.manage", "system.status"],
            "units": 3, "ttl_seconds": 120,
        })
        request = {
            "key": key(), "tool": "system.status", "input": {}, "run_at": timestamp(1.5),
            "deadline_at": timestamp(45), "delegation_id": grant["delegation_ref"]["resource_id"],
        }
        once = node.call("Schedule.Create", request)
        assert once["state"]["lifecycle"] == "Pending", once
        node.stop(signal.SIGKILL)
        binary = current.resolve() / "psiosd"
        before = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert before["database_format"] == 10 and before["migration_required"], before
        after(request["run_at"])
        node.daemon[0] = str(binary)
        node.start()
        first = settled(node, once)
        assert first["lifecycle"] == "Triggered" and first["trigger_id"] == once["state"]["trigger_id"], first
        assert first["task_receipt"]["idempotency_key"] == first["trigger_id"], first
        task_ids = [first["task_receipt"]["task_id"]]
        assert node.call("Schedule.Create", request)["state"] == first
        interval = node.call("Schedule.Create", dict(request, key=key(), run_at=timestamp(0.8), repeat={
            "interval_seconds": 1, "count": 2, "missed": {"mode": "BoundedCatchUp", "limit": 2},
            "overlap": "Allow",
        }))
        final = settled(node, interval)
        assert final["lifecycle"] == "Completed" and len(final["trigger_refs"]) == 2, final
        for ref in final["trigger_refs"]:
            trigger = node.call("Schedule.TriggerGet", {"id": ref["resource_id"]})["state"]
            assert trigger["lifecycle"] == "Triggered", trigger
            task_ids.append(trigger["task_receipt"]["task_id"])
        assert len(set(task_ids)) == 3
        for task in task_ids:
            assert node.wait(task)["lifecycle"] == "Succeeded", task
        node.stop()
        verified = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert verified["database_format"] == 11 and not verified["migration_required"], verified
        old = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        node.start()
        assert settled(node, interval) == final and settled(node, once) == first
        print("PASS format 10 timer migration, settled interval history, unique Tasks and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
