"""Verify registration of the approved rebind tool on a real format-98 authority."""

import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, target


TOOL = "application.rebind_restored_volume"
CAPABILITY = "tool-" + TOOL


def refused(node, code, method, params):
    try:
        node.call(method, params)
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"expected {code} from {method}")


def verify(node):
    result = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert result["sqlite_integrity"] == result["foreign_keys"] == "Verified", result
    assert result["witness"] == "Confirmed", result
    return result


def discover(node):
    return {tool["capability_id"] for tool in node.call("Capability.Discover", {})}


def exercise(previous, current):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "before-rebind-upgrade"})
        content = "retained before rebind registration\n"
        result = node.perform("volume.write", {**target(volume), "name": "evidence", "content": content})
        volume = result["volume_ref"]
        original = node.get(volume["resource_id"])
        request = node.prepare("system.status", {})
        accepted = node.call("Task.Submit", request)
        task = node.wait(accepted["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        artifact, = task["result"]["artifact_refs"]
        proof = node.call("Artifact.Read", {"reference": artifact})
        authority = node.call("Principal.WhoAmI", {})["authority_id"]
        assert proof["authority_id"] == authority, proof

        grant = {"principal_id": "local-owner", "actions": ["task.submit", "task.read", TOOL],
                 "tool_targets": [volume["resource_id"]], "units": 1, "ttl_seconds": 120}
        refused(node, "NotFound", "Resource.Get", {"id": CAPABILITY})
        refused(node, "UnsupportedCapability", "Grant.Issue", grant)
        node.stop(signal.SIGKILL)

        old_daemon = node.daemon[0]
        node.daemon[0] = str((current / "psiosd").resolve())
        before = verify(node)
        assert (before["node_id"], before["database_format"], before["migration_required"]) == (authority, 98, True), before
        node.start()
        capability = node.get(CAPABILITY)
        spec = capability["resource"]["spec"]
        assert capability["resource"]["kind"] == "Capability", capability
        assert capability["value"]["actions"] == [TOOL], capability
        assert spec["requires_approval"] is True and spec["effect_class"] == "NonRetryableEffect", capability
        assert capability["value"]["authority_id"] == authority, capability
        assert TOOL not in discover(node), "migration unexpectedly gave the bootstrap owner this capability"
        refused(node, "InvalidArgument", "Grant.Issue", {k: v for k, v in grant.items() if k != "tool_targets"})
        issued = node.call("Grant.Issue", grant)
        scoped = node.get(issued["grant_ref"]["resource_id"])["value"]
        assert scoped["tool_targets"] == [volume["resource_id"]], scoped
        assert TOOL in discover(node)
        assert node.get(volume["resource_id"]) == original
        assert node.wait(accepted["task_id"]) == task
        assert node.call("Task.Submit", request) == {**accepted, "deduplicated": True}
        assert node.call("Artifact.Read", {"reference": artifact}) == proof
        read = node.perform("volume.read", {**target(volume), "name": "evidence"})
        assert read["content"] == content, read

        node.stop()
        after = verify(node)
        assert after["node_id"] == authority and after["database_format"] >= 99 and not after["migration_required"], after
        fenced = subprocess.run([old_daemon, "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode != 0 and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format 98 to 99 rebind catalog, scoped approval, retained Task and Volume, and old-writer fence")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", type=Path, required=True)
    parser.add_argument("--current-bin-dir", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin_dir, args.current_bin_dir)
