"""Install the volume usage tool catalog on an actual format-14 volume."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, key, target


def rejected(node, code, method, request):
    try:
        node.call(method, request)
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"expected {code} from {method}")


def upgrade(previous, current):
    node = Node(previous)
    try:
        owner = node.call("Principal.WhoAmI", {})["authority_id"]
        volume = node.perform("volume.create", {"name": "existing-data"})
        root = node.state / "volumes" / volume["resource_id"] / "data"
        data = root / "existing"
        data.write_bytes(b"existing contents")
        params = {"target": target(volume)}
        base = {"key": key(), "input": params, "tool": "volume.usage", "delegation_id": "owner-delegation"}
        grant_params = {"principal_id": "local-owner", "units": 1, "ttl_seconds": 60,
                        "tool_targets": [volume["resource_id"]], "actions": ["volume.usage", "task.submit", "task.read"]}
        rejected(node, "UnsupportedCapability", "Grant.Issue", grant_params)
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        before = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert before["database_format"] == 14 and before["migration_required"], before
        old = node.daemon[0]
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Principal.WhoAmI", {})["authority_id"] == owner
        assert node.get(volume["resource_id"])["reference"] == volume
        rejected(node, "PermissionDenied", "Task.Prepare", base)
        grant = node.call("Grant.Issue", grant_params)
        request = node.call("Task.Prepare", dict(base, delegation_id=grant["delegation_ref"]["resource_id"]))
        receipt = node.call("Task.Submit", request)
        task = node.wait(receipt["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        usage = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        assert usage["volume_ref"] == volume and usage["files"] == 1, usage
        assert int(usage["logical_file_bytes"]) == data.stat().st_size, usage
        assert int(usage["allocated_file_bytes"]) == data.stat().st_blocks * 512, usage
        node.stop()
        after = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert after["database_format"] >= 15 and not after["migration_required"], after
        fenced = subprocess.run([old, "--config", node.config, "check"], capture_output=True, text=True, timeout=15)
        assert fenced.returncode != 0 and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format 14 migration, unchanged volume identity, explicit tool scope and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    opts = parser.parse_args()
    upgrade(opts.previous_bin, opts.current_bin)
