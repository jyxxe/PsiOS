"""Keep exact older scopes while publishing a new node mount tool on upgrade."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, key


def refused(node, code, delegation):
    try:
        node.prepare("node.mounts", {}, key(), delegation=delegation)
    except Failure as failure:
        assert failure.code == code, failure
    else:
        raise AssertionError("old scope allowed the mount tool")


def upgrade(previous, current):
    node = Node(previous)
    try:
        identity = node.call("Principal.WhoAmI", {})["authority_id"]
        params = {"principal_id": "local-owner", "units": 4, "ttl_seconds": 120,
                  "actions": ["task.submit", "task.read", "node.inventory"], "tool_targets": [identity]}
        grant = node.call("Grant.Issue", params)
        original = grant["delegation_ref"]["resource_id"]
        receipt = node.call("Task.Submit", node.prepare("node.inventory", {}, key(), delegation=original))
        done = node.wait(receipt["task_id"])
        assert done["lifecycle"] == "Succeeded", done
        refused(node, "UnsupportedCapability", original)
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        before = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert before["database_format"] == 15 and before["migration_required"], before
        old = node.daemon[0]
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Principal.WhoAmI", {})["authority_id"] == identity
        assert node.call("Task.Get", {"id": receipt["task_id"]}) == done
        refused(node, "PermissionDenied", original)
        refused(node, "PermissionDenied", "owner-delegation")
        grant = node.call("Grant.Issue", dict(params, actions=["task.submit", "task.read", "node.mounts"]))
        request = node.prepare("node.mounts", {}, key(), delegation=grant["delegation_ref"]["resource_id"])
        receipt = node.call("Task.Submit", request)
        task = node.wait(receipt["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        report = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        assert report["mount_namespace_inode"] == str(Path("/proc/self/ns/mnt").stat().st_ino)
        assert any(mount["mount_point"] == "/" for mount in report["mounts"])
        node.stop()
        after = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert after["database_format"] >= 16 and not after["migration_required"], after
        fenced = subprocess.run([old, "--config", node.config, "check"], capture_output=True, text=True, timeout=15)
        assert fenced.returncode != 0 and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format 15 migration, untouched inventory grant and receipt, restricted mounts and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    opts = parser.parse_args()
    upgrade(opts.previous_bin, opts.current_bin)
