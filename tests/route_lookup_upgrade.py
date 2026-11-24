"""Upgrade a real format 9 authority with a pending scheduled Task."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Failure, Node, key
from test_schedule import after, timestamp


def upgrade(previous, current):
    n = Node(previous)
    try:
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        old_grant = n.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=[node],
                          actions=["task.submit", "task.read", "node.routes"], units=1, ttl_seconds=120))
        scheduled_grant = n.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=["profile-local"],
            actions=["task.submit", "task.read", "schedule.manage", "system.status"], units=1, ttl_seconds=120))
        params = dict(key=key(), tool="system.status", input={}, run_at=timestamp(1.5), deadline_at=timestamp(25),
                      delegation_id=scheduled_grant["delegation_ref"]["resource_id"])
        scheduled = n.call("Schedule.Create", params)
        assert scheduled["state"]["lifecycle"] == "Pending", scheduled
        query = {"destination": "127.0.0.1"}
        try:
            n.prepare("node.route_lookup", query)
        except Failure as error:
            assert error.code == "UnsupportedCapability", error
        else:
            raise AssertionError("format 9 already registered the new lookup tool")
        n.stop(signal.SIGKILL)
        after(params["run_at"])
        binary = str((current / "psiosd").resolve())
        old_report = json.loads(subprocess.check_output([binary, "--config", n.config, "verify"]))
        assert old_report["database_format"] == 9 and old_report["migration_required"], old_report
        n.daemon[0] = binary
        n.start()
        assert n.call("Node.Get", {})["reference"]["resource_id"] == node
        delegation = old_grant["delegation_ref"]
        assert n.get(delegation["resource_id"])["reference"] == delegation
        try:
            n.prepare("node.route_lookup", query, delegation=delegation["resource_id"])
        except Failure as error:
            assert error.code == "PermissionDenied", error
        else:
            raise AssertionError("upgrade widened an existing routing grant")
        grant = n.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=[node],
                       actions=["task.submit", "task.read", "node.route_lookup"], units=1, ttl_seconds=120))
        prepared = n.prepare("node.route_lookup", query, delegation=grant["delegation_ref"]["resource_id"])
        lookup = n.wait(n.call("Task.Submit", prepared)["task_id"])
        assert lookup["lifecycle"] == "Succeeded", lookup
        ref, = lookup["result"]["artifact_refs"]
        result = n.call("Artifact.Read", {"reference": ref})
        assert result["outcome"] == "Resolved" and result["route"]["table"] == 255, result
        status_request = {"id": scheduled["reference"]["resource_id"]}
        for _ in range(100):
            state = n.call("Schedule.Get", status_request)["state"]
            if state["lifecycle"] != "Pending":
                break
            time.sleep(0.05)
        assert state["lifecycle"] == "Triggered" and state["trigger_id"] == scheduled["state"]["trigger_id"], state
        assert n.wait(state["task_receipt"]["task_id"])["lifecycle"] == "Succeeded"
        n.stop()
        report = json.loads(subprocess.check_output([binary, "--config", n.config, "verify"]))
        assert report["database_format"] == 10 and not report["migration_required"], report
        old = subprocess.run([previous.resolve() / "psiosd", "--config", n.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        n.start()
        assert n.call("Artifact.Read", {"reference": ref}) == result
        assert n.call("Schedule.Get", status_request)["state"] == state
        print("PASS format 9 lookup catalog upgrade, preserved pending timer and grant scope, old-writer refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
