"""Keep a pending installation pinned through catalog upgrades."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, target
from test_artifact_install import TOOL, grant, prepare, publish_install


def still_pinned(node, request):
    try:
        node.call("Artifact.Withdraw", request)
    except Failure as error:
        assert error.code == "Conflict", error
    else:
        raise AssertionError("pending installation lost its source pin")


def main(previous, previous_format):
    node = Node(previous)
    try:
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        grant(node, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
        reference = publish_install(node, {"name": "pending-release", "image_id": "sha256:" + "b" * 64,
            "argv": ["/probe"], "release_version": "1.0.0", "license_expression": "0BSD",
            "cpu_millicores": 200, "memory_max_bytes": 32 * 1024 * 1024,
            "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024})
        permitted = grant(node, "local-owner", ["task.submit", "task.read", TOOL], ["profile-local", reference["resource_id"]])
        request = prepare(node, reference, permitted)["request"]
        job = node.call("Task.Submit", request)["task_id"]
        before = node.wait(job)
        assert before["lifecycle"] == "WaitingApproval", before
        withdrawal = {"reference": reference, "reason": "installation cancelled after upgrade"}
        still_pinned(node, withdrawal)

        node.stop(signal.SIGKILL)
        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        prior = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert prior["database_format"] == previous_format and prior["migration_required"], prior
        node.start()
        observed = node.call("Task.Get", {"id": job})
        assert observed == before, observed
        assert node.call("Task.Submit", request)["task_id"] == job
        still_pinned(node, withdrawal)
        owner = node.get("owner-delegation")["value"]
        assert "application.retire_release" not in owner["actions"], owner
        assert "application.stage_artifact_release" not in owner["actions"], owner
        assert "application.releases" not in owner["actions"], owner
        node.call("Task.Cancel", target(node.get(job)["reference"]))
        assert node.call("Artifact.Withdraw", withdrawal)["withdrawal_ref"]
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] > previous_format and not after["migration_required"], after
        assert prior["node_id"] == after["node_id"] == node_id
        print(f"PASS format {previous_format} pending installation preserved its approval, receipt and pin across catalog upgrade")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    parser.add_argument("--previous-format", type=int, default=55)
    args = parser.parse_args()
    main(args.previous_bin, args.previous_format)
