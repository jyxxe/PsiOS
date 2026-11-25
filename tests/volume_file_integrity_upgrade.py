"""Register the new file inspection capability without widening a format-47 grant."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

from support import Failure, Node, target


def main(previous):
    node = Node(previous)
    try:
        actor, legacy = node.agent(["task.submit", "task.read", "volume.create", "volume.read"])
        token = actor["token"]
        delegation = legacy["delegation_ref"]["resource_id"]
        request = node.prepare("volume.create", {"name": "old-file-proof"}, token=token, delegation=delegation)
        created = node.wait(node.call("Task.Submit", request, token)["task_id"])
        volume = node.call("Artifact.Read", {"reference": created["result"]["artifact_refs"][0]}, token)
        data = node.state / "volumes" / volume["resource_id"] / "data"
        contents = b"\x00before integrity verification\xff"
        (data / "file").write_bytes(contents)
        node.stop()

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 47 and before["migration_required"], before
        node.start()
        args = {"target": target(volume), "path": "file", "expected_digest": "sha256:" + hashlib.sha256(contents).hexdigest()}
        try:
            node.prepare("volume.verify_file", args, token=token, delegation=delegation)
        except Failure as denied:
            assert denied.code == "PermissionDenied", denied
        else:
            raise AssertionError("upgrade widened a legacy actor grant")
        grant = node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "units": 2, "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "volume.verify_file"], "tool_targets": [volume["resource_id"]],
        })
        prepared = node.prepare("volume.verify_file", args, token=token,
                                delegation=grant["delegation_ref"]["resource_id"])
        task = node.wait(node.call("Task.Submit", prepared, token)["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        proof = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)
        assert proof["content_digest"] == args["expected_digest"] and proof["volume_ref"] == volume, proof
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 48 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 47 volume and legacy grant retained; scoped file integrity verified original bytes")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
