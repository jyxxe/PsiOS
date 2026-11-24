"""Upgrade a format 8 Core and dispatch a scheduled Task under the same identity."""
import argparse
import json
from pathlib import Path
import subprocess
import time

from support import Failure, Node, key
from test_schedule import timestamp


def upgrade(previous, current):
    n = Node(previous)
    try:
        identity = n.call("Node.Get", {})["reference"]["resource_id"]
        owner = n.get("owner-delegation")["reference"]
        prior = n.wait(n.submit("system.status", {}))
        assert prior["lifecycle"] == "Succeeded", prior
        actions = ["task.submit", "task.read", "schedule.manage", "system.status"]
        grant = dict(principal_id="local-owner", tool_targets=["profile-local"], actions=actions,
                     ttl_seconds=60, units=1)
        try:
            n.call("Grant.Issue", grant)
        except Failure as error:
            assert error.code == "UnsupportedCapability", error
        else:
            raise AssertionError("format 8 accepted the new schedule scope")
        n.stop()
        binary = str((current / "psiosd").resolve())
        before = json.loads(subprocess.check_output([binary, "--config", n.config, "verify"]))
        assert before["database_format"] == 8 and before["migration_required"], before
        n.daemon[0] = binary
        n.start()
        assert n.call("Node.Get", {})["reference"]["resource_id"] == identity
        assert n.get("owner-delegation")["reference"] == owner
        assert n.wait(prior["task_id"])["result"] == prior["result"]
        permission = n.call("Grant.Issue", grant)["delegation_ref"]["resource_id"]
        schedule = n.call("Schedule.Create", dict(key=key(), tool="system.status", input={}, delegation_id=permission,
                          run_at=timestamp(0.75), deadline_at=timestamp(15)))
        request = {"id": schedule["reference"]["resource_id"]}
        for _ in range(100):
            result = n.call("Schedule.Get", request)["state"]
            if result["lifecycle"] != "Pending":
                break
            time.sleep(0.05)
        assert result["lifecycle"] == "Triggered", result
        assert n.wait(result["task_receipt"]["task_id"])["lifecycle"] == "Succeeded"
        n.stop()
        after = json.loads(subprocess.check_output([binary, "--config", n.config, "verify"]))
        assert after["database_format"] == 9 and not after["migration_required"], after
        old = subprocess.run([previous.resolve() / "psiosd", "--config", n.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        n.start()
        assert n.call("Schedule.Get", request)["state"] == result
        print("PASS format 8 schedule migration, original principal and results, timed Task, and old-writer refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
