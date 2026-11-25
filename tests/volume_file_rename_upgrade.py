"""Use a format-49 observation to approve an atomic rename after upgrading."""
import argparse
import json
from pathlib import Path
import subprocess

from support import Node, target


def main(previous):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "old-file-observation"})
        data = node.state / "volumes" / volume["resource_id"] / "data"
        source = data / "before"
        source.write_bytes(b"observed by the original authority")
        inode = source.stat().st_ino
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", "volume.verify_file"],
            "units": 2, "ttl_seconds": 120, "tool_targets": [volume["resource_id"]],
        })
        request = node.prepare("volume.verify_file", {"target": target(volume), "path": "before"},
                               delegation=grant["delegation_ref"]["resource_id"])
        result = node.wait(node.call("Task.Submit", request)["task_id"])
        reference, = result["result"]["artifact_refs"]
        proof = node.call("Artifact.Read", {"reference": reference})
        node.stop()

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 49 and before["migration_required"], before
        node.start()
        assert node.call("Artifact.Read", {"reference": reference}) == proof
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", "volume.rename_file"],
            "units": 2, "ttl_seconds": 120, "tool_targets": [volume["resource_id"]],
        })
        request = node.prepare("volume.rename_file", {
            "target": target(volume), "source": "before", "destination": "after",
            "expected_observation_digest": proof["observation_digest"],
        }, delegation=grant["delegation_ref"]["resource_id"])
        task = node.call("Task.Submit", request)["task_id"]
        settled = node.wait(task, autoapprove=True)
        assert settled["lifecycle"] == "Succeeded", settled
        assert (data / "after").stat().st_ino == inode and not source.exists()
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 50 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 49 file proof bound an approved rename of its original inode")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
