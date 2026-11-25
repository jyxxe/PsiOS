"""Install the launch capability without widening an actual format 32 grant."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Failure, Node
from test_schedule import timestamp


def mode(node, scheduling):
    current = node.call("Node.Get", {})["reference"]
    return node.call("Node.Configure", {
        "expected_resource_version": current["version"], "scheduling": scheduling,
    })


def main(previous):
    n = Node(previous)
    try:
        old_actions = ["workflow.manage", "schedule.manage", "task.submit", "task.read",
                       "system.status", "resource.read", "event.read"]
        old = n.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 2, "ttl_seconds": 90,
            "actions": old_actions, "tool_targets": ["profile-local"],
        })
        definition = n.call("Workflow.Define", {"key": "original", "steps": [
            {"id": "observe", "tool": "system.status", "input": {}, "after": []},
        ]})["reference"]
        source = mode(n, "Cordoned")
        previous_rule = n.call("Schedule.Create", {
            "key": "pending-event-from-32", "tool": "system.status", "input": {},
            "delegation_id": old["delegation_ref"]["resource_id"],
            "event": {"source_ref": source, "condition": {"kind": "SpecChanged"},
                      "expires_at": timestamp(35), "max_triggers": 1},
        })
        rule_query = {"id": previous_rule["reference"]["resource_id"]}
        n.stop(signal.SIGKILL)

        current = Path("target/debug").absolute()
        n.daemon[0] = str(current / "psiosd")
        before = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 32 and before["migration_required"], before
        n.start()
        assert n.call("Schedule.Get", rule_query) == previous_rule
        blueprint = {"definition_ref": definition, "max_runtime_seconds": 25}
        try:
            n.prepare("workflow.start", blueprint, delegation=old["delegation_ref"]["resource_id"])
        except Failure as error:
            assert error.code == "PermissionDenied", error
        else:
            raise AssertionError("migration implicitly authorized a launch")
        fresh = n.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 2, "ttl_seconds": 90,
            "actions": ["workflow.start", "system.status", "workflow.manage", "task.submit", "task.read"],
            "tool_targets": ["profile-local", definition["resource_id"]],
        })
        request = n.prepare("workflow.start", blueprint, delegation=fresh["delegation_ref"]["resource_id"])
        receipt = n.call("Task.Submit", request)
        mode(n, "Enabled")
        admitted = n.wait(receipt["task_id"])
        assert admitted["lifecycle"] == "Succeeded", admitted
        launch = n.call("Artifact.Read", {"reference": admitted["result"]["artifact_refs"][0]})
        limit = time.monotonic() + 10
        while True:
            event = n.call("Schedule.Get", rule_query)["state"]
            run = n.call("Workflow.Get", {"id": launch["workflow_run_ref"]["resource_id"]})
            if event["lifecycle"] == "Completed" and run["state"]["lifecycle"] == "Succeeded":
                break
            assert time.monotonic() < limit, (event, run)
            time.sleep(.05)
        assert run["state"]["origin_task_ref"] == launch["origin_task_ref"]
        trigger = n.call("Schedule.TriggerGet", {"id": event["trigger_refs"][0]["resource_id"]})["state"]
        assert n.wait(trigger["task_receipt"]["task_id"])["lifecycle"] == "Succeeded"
        n.stop()
        after = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert after["node_id"] == before["node_id"]
        assert after["database_format"] >= 33 and not after["migration_required"], after
        print("PASS format 32 kept the queued event and original scope; explicit grant launched a workflow")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
