"""Carry a pending literal workflow through the condition-aware format fence."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Node, target
from test_schedule import timestamp
from test_workflow import step


def wait_workflow(node, identity, phase):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        run = node.call("Workflow.Get", {"id": identity})
        if run["state"]["lifecycle"] == phase:
            return run
        assert run["state"]["lifecycle"] not in ("Succeeded", "Failed", "Cancelled"), run
        time.sleep(0.05)
    raise AssertionError(run)


def exercise(previous, current):
    node = Node(previous)
    try:
        identity = node.call("Principal.WhoAmI", {})["authority_id"]
        volume = node.perform("volume.create", {"name": "legacy-workflow"})
        grant = node.call("Grant.Issue", {"principal_id": "local-owner",
            "actions": ["workflow.manage", "system.status", "volume.write", "task.submit", "task.read"],
            "tool_targets": ["profile-local", volume["resource_id"]], "units": 5, "ttl_seconds": 180})
        definition = {"key": "legacy-definition", "steps": [
            step("write", "volume.write", dict(**target(volume), name="before", content="retained")),
            step("report", after=["write"]),
        ]}
        frozen = node.call("Workflow.Define", definition)
        assert frozen["definition"]["dsl_version"] == 1
        run = node.call("Workflow.Start", {"key": "legacy-run", "definition_ref": frozen["reference"],
            "delegation_id": grant["delegation_ref"]["resource_id"], "deadline_at": timestamp(145)})
        run_id = run["reference"]["resource_id"]
        before = wait_workflow(node, run_id, "WaitingApproval")
        task = before["state"]["steps"][0]["task_receipt"]["task_id"]
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        verify = [binary, "--config", node.config, "verify"]
        observation = json.loads(subprocess.check_output(verify, timeout=15))
        assert (observation["node_id"], observation["database_format"], observation["migration_required"]) == (
            identity, 23, True), observation
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Workflow.Define", definition) == frozen
        assert wait_workflow(node, run_id, "WaitingApproval") == before
        node.approve(task)
        completed = wait_workflow(node, run_id, "Succeeded")
        assert completed["state"]["steps"][0]["task_receipt"]["task_id"] == task
        path = node.state / "volumes" / volume["resource_id"] / "data" / "before"
        assert path.read_text() == "retained"

        selected = node.call("Workflow.Define", {"key": "new-definition", "steps": [step("source"),
            step("skip", after=["source"], when={"step": "source", "pointer": "/authority_id", "equals": "different"})]})
        assert selected["definition"]["dsl_version"] == 2
        new = node.call("Workflow.Start", {"key": "new-run", "definition_ref": selected["reference"],
            "delegation_id": grant["delegation_ref"]["resource_id"], "deadline_at": timestamp(120)})
        completed = wait_workflow(node, new["reference"]["resource_id"], "Succeeded")
        chosen = completed["state"]["steps"][1]
        assert chosen["lifecycle"] == "Skipped" and chosen["condition"]["selected"] is False
        assert chosen["task_receipt"] is None

        node.stop()
        observation = json.loads(subprocess.check_output(verify, timeout=15))
        assert observation["database_format"] >= 24 and not observation["migration_required"], observation
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS legacy workflow approval resumed, conditional selection persisted and old writer fenced")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
