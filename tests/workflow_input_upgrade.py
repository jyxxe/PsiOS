"""Resume a format 28 conditional write before enabling artifact input mappings."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, target
from test_schedule import timestamp
from test_workflow import step
from workflow_conditions_upgrade import wait_workflow


def exercise(previous, current):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "workflow-mapping-upgrade"})
        identity = node.call("Principal.WhoAmI", {})["authority_id"]
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 2, "ttl_seconds": 120,
            "actions": ["workflow.manage", "system.status", "volume.write", "task.submit", "task.read"],
            "tool_targets": ["profile-local", volume["resource_id"]]})
        definition = node.call("Workflow.Define", {"key": "legacy-dsl", "steps": [step("source"),
            step("write", "volume.write", dict(**target(volume), name="record", content="pre-upgrade"),
                after=["source"], when={"step": "source", "pointer": "/authority_id", "equals": identity})]})
        assert definition["definition"]["dsl_version"] == 2
        started = node.call("Workflow.Start", {"key": "legacy-run", "definition_ref": definition["reference"],
            "delegation_id": grant["delegation_ref"]["resource_id"], "deadline_at": timestamp(90)})
        run = started["reference"]["resource_id"]
        waiting = wait_workflow(node, run, "WaitingApproval")
        task = waiting["state"]["steps"][1]["task_receipt"]["task_id"]
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        verify = [binary, "--config", node.config, "verify"]
        before = json.loads(subprocess.check_output(verify, timeout=15))
        assert before["database_format"] == 28 and before["migration_required"], before
        node.daemon[0] = str(binary)
        node.start()
        assert wait_workflow(node, run, "WaitingApproval") == waiting
        node.approve(task)
        complete = wait_workflow(node, run, "Succeeded")
        assert complete["state"]["steps"][1]["condition"] == waiting["state"]["steps"][1]["condition"]
        assert (node.state / "volumes" / volume["resource_id"] / "data" / "record").read_text() == "pre-upgrade"
        node.stop()

        after = json.loads(subprocess.check_output(verify, timeout=15))
        assert after["node_id"] == identity and after["database_format"] >= 29 and not after["migration_required"], after
        rejected = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                  capture_output=True, text=True, timeout=15)
        assert rejected.returncode and "SchemaVersionUnsupported" in rejected.stderr, rejected
        print("PASS format 28 conditional selection and approval resume, older writer fenced")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
