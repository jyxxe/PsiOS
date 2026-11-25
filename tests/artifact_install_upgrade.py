"""Prepare a restorable upload from format 54 without extending existing grants."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, target
from test_artifact_install import TOOL, grant, prepare, publish_install


def main(previous):
    node = Node(previous)
    try:
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        prior = grant(node, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
        source = publish_install(node, {"name": "upgrade-artifact", "image_id": "sha256:" + "b" * 64,
            "argv": ["/probe"], "release_version": "1.0.0", "license_expression": "0BSD",
            "cpu_millicores": 200, "memory_max_bytes": 32 * 1024 * 1024,
            "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024})
        node.stop(signal.SIGKILL)
        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 54 and before["migration_required"], before
        node.start()
        try:
            prepare(node, source, prior)
        except Failure as denied:
            assert denied.code == "PermissionDenied", denied
        else:
            raise AssertionError("an upload grant acquired installation rights after migration")
        allowed = grant(node, "local-owner", ["task.submit", "task.read", TOOL], ["profile-local", source["resource_id"]])
        request = prepare(node, source, allowed)["request"]
        task = node.wait(node.call("Task.Submit", request)["task_id"])
        assert task["lifecycle"] == "WaitingApproval", task
        reference = node.get(task["task_id"])["reference"]
        node.call("Task.Cancel", target(reference))
        assert node.call("Artifact.Withdraw", {"reference": source, "reason": "cancelled upgraded install"})["withdrawal_ref"]
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 55 and not after["migration_required"], after
        assert before["node_id"] == after["node_id"] == node_id
        print("PASS format 54 upload fitted an approved installation profile with explicit scope and released its cancelled pin")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
