"""Upgrade the last format 5 authority and exercise its newly registered route tool."""
import argparse
import json
from pathlib import Path
import subprocess

from support import Failure, Node


def upgrade(previous, current):
    node = Node(previous)
    try:
        identity = node.call("Node.Get", {})["reference"]["resource_id"]
        prior = node.submit("node.inventory", {})
        assert node.wait(prior)["lifecycle"] == "Succeeded"
        old_grant = node.get("owner-delegation")["reference"]
        node.stop()
        binary = str((current / "psiosd").resolve())
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] == 5 and report["migration_required"], report
        node.daemon[0] = binary
        node.start()
        assert node.call("Node.Get", {})["reference"]["resource_id"] == identity
        assert node.get("owner-delegation")["reference"] == old_grant
        assert node.wait(prior)["lifecycle"] == "Succeeded"
        try:
            node.prepare("node.routes", {})
        except Failure as error:
            assert error.code == "PermissionDenied", error
        else:
            raise AssertionError("upgrade widened a format 5 grant")
        grant = node.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=[identity],
                          actions=["task.submit", "task.read", "node.routes"], units=1, ttl_seconds=120))
        request = node.prepare("node.routes", {"family": "inet"}, delegation=grant["delegation_ref"]["resource_id"])
        task = node.wait(node.call("Task.Submit", request)["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        report = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        assert report["boot_id"] and "routes" in report
        node.stop()
        old = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        node.start()
        assert node.wait(task["task_id"])["result"] == task["result"]
        print("PASS format 5 route catalog upgrade, durable receipts, grant scope and downgrade refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
