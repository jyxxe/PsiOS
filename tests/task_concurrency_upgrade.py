"""Preserve queued work and its original budget across format 39 migration."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node


def configure(node, mode):
    state = node.call("Node.Get", {})
    node.call("Node.Configure", {
        "expected_resource_version": state["reference"]["version"], "scheduling": mode,
    })


def main(previous):
    node = Node(previous)
    try:
        configure(node, "Cordoned")
        actor = node.call("Agent.Register", {"name": "uncapped-before-upgrade"})
        grant = node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "units": 2, "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "system.status"],
        })
        budget_id = grant["budget_ref"]["resource_id"]
        request = node.prepare("system.status", {}, token=actor["token"],
                               delegation=grant["delegation_ref"]["resource_id"])
        task_id = node.call("Task.Submit", request, actor["token"])["task_id"]
        queued = node.call("Task.Get", {"id": task_id})
        old_budget = node.call("Budget.Get", {"id": budget_id})
        assert queued["lifecycle"] == "Queued" and old_budget["reserved"] == 1
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 39 and before["migration_required"], before
        node.start()
        assert node.call("Task.Get", {"id": task_id}) == queued
        assert node.call("Budget.Get", {"id": budget_id}) == old_budget
        configure(node, "Enabled")
        assert node.wait(task_id)["lifecycle"] == "Succeeded"
        budget = node.call("Budget.Get", {"id": budget_id})
        assert (budget["reserved"], budget["spent"]) == (0, 1), budget
        assert "task_concurrency_maximum" not in budget, budget
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 40 and not after["migration_required"], after
        assert before["node_id"] == after["node_id"]
        print("PASS format 39 retained the queued task and its original uncapped budget")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
