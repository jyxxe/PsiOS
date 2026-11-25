"""Migrate a format 37 WorkflowRun with its original approved-task boundary."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Node, key, target
from test_schedule import timestamp
from test_workflow import step


def wait_run(node, ref, waiting):
    deadline = time.monotonic() + 10
    while True:
        view = node.call("Workflow.Get", {"id": ref["resource_id"]})
        if view["state"]["lifecycle"] == waiting:
            return view
        assert time.monotonic() < deadline, view
        time.sleep(0.05)


def main(previous):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "serial-upgrade"})
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 2, "ttl_seconds": 120,
            "tool_targets": ["profile-local", volume["resource_id"]],
            "actions": ["workflow.manage", "task.submit", "task.read", "volume.write", "system.status"],
        })
        definition = node.call("Workflow.Define", {"key": key(), "steps": [
            step("write", "volume.write", {**target(volume), "name": "original", "content": "captured"}),
            step("after", after=["write"]),
        ]})
        instance = node.call("Workflow.Start", {"key": key(), "definition_ref": definition["reference"],
            "delegation_id": grant["delegation_ref"]["resource_id"], "deadline_at": timestamp(45),
        })
        ref = instance["reference"]
        pending = wait_run(node, ref, "WaitingApproval")
        assert pending["state"]["active_step"] == "write" and "active_steps" not in pending["state"]
        task = pending["state"]["steps"][0]["task_receipt"]["task_id"]
        approval = node.wait(task)
        assert approval["lifecycle"] == "WaitingApproval", approval
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 37 and before["migration_required"], before
        node.start()
        assert wait_run(node, ref, "WaitingApproval") == pending
        assert node.wait(task) == approval
        node.approve(task)
        final = wait_run(node, ref, "Succeeded")
        write, subsequent = final["state"]["steps"]
        assert write["task_receipt"]["task_id"] == task and subsequent["lifecycle"] == "Succeeded", final
        data = node.state / "volumes" / volume["resource_id"] / "data" / "original"
        assert data.read_text() == "captured"
        budget = node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        assert (budget["reserved"], budget["spent"]) == (0, 2), budget
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 38 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 37 kept its WorkflowRun, approval, Task IDs and budget through parallel scheduler migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
