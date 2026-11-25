"""Keep a pending timed schedule and admit a resource trigger after a format 31 upgrade."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Node
from test_schedule import timestamp


def mode(n, scheduling):
    node = n.call("Node.Get", {})["reference"]
    return n.call("Node.Configure", {
        "expected_resource_version": node["version"], "scheduling": scheduling,
    })


def main(previous):
    n = Node(previous)
    try:
        old = n.wait(n.submit("system.status", {}))
        previous_result = old["result"]["artifact_refs"][0]
        content = n.call("Artifact.Read", {"reference": previous_result})
        mode(n, "Cordoned")
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "tool_targets": ["profile-local"], "units": 2,
            "ttl_seconds": 90, "actions": [
                "system.status", "schedule.manage", "task.submit", "task.read", "event.read", "resource.read",
            ],
        })
        common = {"tool": "system.status", "input": {},
                  "delegation_id": grant["delegation_ref"]["resource_id"]}
        timed = n.call("Schedule.Create", dict(common, key="prior-clock", run_at=timestamp(1),
                                               deadline_at=timestamp(35)))
        assert timed["state"]["lifecycle"] == "Pending"
        timed_id = {"id": timed["reference"]["resource_id"]}
        n.stop(signal.SIGKILL)

        current = Path("target/debug").absolute()
        n.daemon[0] = str(current / "psiosd")
        before = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 31 and before["migration_required"], before
        n.start()
        assert n.call("Schedule.Get", timed_id) == timed
        assert n.call("Artifact.Read", {"reference": previous_result}) == content
        source = n.call("Node.Get", {})["reference"]
        new = n.call("Schedule.Create", dict(common, key="post-upgrade-event", event={
            "source_ref": source, "condition": {"kind": "SpecChanged"},
            "expires_at": timestamp(25), "max_triggers": 1,
        }))
        enabled = mode(n, "Enabled")
        event_id = {"id": new["reference"]["resource_id"]}
        limit = time.monotonic() + 10
        while True:
            timer = n.call("Schedule.Get", timed_id)["state"]
            event = n.call("Schedule.Get", event_id)["state"]
            if timer["lifecycle"] == "Triggered" and event["lifecycle"] == "Completed":
                break
            assert time.monotonic() < limit, (timer, event)
            time.sleep(.05)
        trigger = n.call("Schedule.TriggerGet", {"id": event["trigger_refs"][0]["resource_id"]})["state"]
        assert trigger["source_event"]["resource_ref"] == enabled
        for receipt in (timer["task_receipt"], trigger["task_receipt"]):
            assert n.wait(receipt["task_id"])["lifecycle"] == "Succeeded"
        n.stop()
        after = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert after["node_id"] == before["node_id"]
        assert after["database_format"] >= 32 and not after["migration_required"], after
        print("PASS format 31 retained artifacts and timed schedule alongside a resource event schedule")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
