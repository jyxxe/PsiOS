"""Migrate an actual format-11 authority with an unfinished interval schedule."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Failure, Node, key
from test_schedule import timestamp
from test_workflow import step


def wait(node, method, resource, terminal):
    deadline = time.monotonic() + 12
    while True:
        record = node.call(method, {"id": resource["reference"]["resource_id"]})
        if record["state"]["lifecycle"] in terminal:
            return record
        assert time.monotonic() < deadline, record
        time.sleep(0.05)


def upgrade(previous, current):
    node = Node(previous)
    try:
        grant = {
            "principal_id": "local-owner", "tool_targets": ["profile-local"],
            "actions": ["task.submit", "task.read", "system.status"], "units": 2, "ttl_seconds": 120,
        }
        try:
            node.call("Grant.Issue", dict(grant, actions=[*grant["actions"], "workflow.manage"]))
        except Failure as error:
            assert error.code == "UnsupportedCapability", error
        else:
            raise AssertionError("old authority recognized the workflow action")
        timer = node.call("Grant.Issue", dict(grant, actions=[*grant["actions"], "schedule.manage"]))
        pending = node.call("Schedule.Create", {
            "key": key(), "tool": "system.status", "input": {},
            "delegation_id": timer["delegation_ref"]["resource_id"], "run_at": timestamp(1.5),
            "deadline_at": timestamp(20), "repeat": {
                "interval_seconds": 1, "count": 2, "overlap": "Allow",
                "missed": {"mode": "BoundedCatchUp", "limit": 2},
            },
        })
        node.stop(signal.SIGKILL)
        binary = current.resolve() / "psiosd"
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] == 11 and report["migration_required"], report
        node.daemon[0] = str(binary)
        node.start()
        workflow_grant = node.call("Grant.Issue", dict(grant, actions=[*grant["actions"], "workflow.manage"]))
        definition = node.call("Workflow.Define", {"key": key(), "steps": [step("first"), step("last", after=["first"])]})
        request = {"key": key(), "definition_ref": definition["reference"], "deadline_at": timestamp(20),
                   "delegation_id": workflow_grant["delegation_ref"]["resource_id"]}
        workflow = node.call("Workflow.Start", request)
        settled = wait(node, "Schedule.Get", pending, {"Completed", "Rejected"})
        assert settled["state"]["lifecycle"] == "Completed", settled
        refs = settled["state"]["trigger_refs"]
        assert len(refs) == 2 and refs[0]["resource_id"] == pending["state"]["next"]["trigger_id"], refs
        result = wait(node, "Workflow.Get", workflow, {"Succeeded", "Failed", "Cancelled"})
        assert (result["state"]["lifecycle"], result["state"]["outcome_knowledge"]) == ("Succeeded", "Known"), result
        task_ids = [s["task_receipt"]["task_id"] for s in result["state"]["steps"]]
        for ref in refs:
            trigger = node.call("Schedule.TriggerGet", {"id": ref["resource_id"]})["state"]
            assert trigger["lifecycle"] == "Triggered", trigger
            task_ids.append(trigger["task_receipt"]["task_id"])
        assert len(set(task_ids)) == 4
        assert all(node.wait(task)["lifecycle"] == "Succeeded" for task in task_ids)
        node.stop()
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] >= 12 and not report["migration_required"], report
        old = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        node.start()
        assert node.call("Workflow.Start", request) == result
        print("PASS format 11 migration, retained interval identity, durable workflow and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    opts = parser.parse_args()
    upgrade(opts.previous_bin, opts.current_bin)
