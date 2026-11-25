"""Keep open, same-owner result dependencies intact when adding scoped readers."""
import argparse
import hashlib
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key, target


def schedule(node, state):
    ref = node.call("Node.Get", {})["reference"]
    node.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": state})


def main(previous):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "before-reader-scope"})
        data = node.state / "volumes" / volume["resource_id"] / "data"
        (data / "input").write_text("legacy producer")
        result = {"name": "input", "content": "legacy producer"}
        expected = "sha256:" + hashlib.sha256(
            json.dumps(result, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
        schedule(node, "Cordoned")
        upstream = node.submit("volume.read", {**target(volume), "name": "input"})
        prepared = node.call("Task.Prepare", {
            "key": key(), "tool": "system.status", "input": {}, "delegation_id": "owner-delegation",
            "requires": [{"task_id": upstream, "digest": expected}],
        })
        dependent = node.call("Task.Submit", prepared)["task_id"]
        waiting = node.call("Task.Get", {"id": dependent})
        assert waiting["lifecycle"] == "WaitingResource", waiting
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 44 and before["migration_required"], before
        node.start()
        assert node.call("Task.Get", {"id": dependent}) == waiting
        reader = node.call("Agent.Register", {"name": "upgrade-reader"})
        node.call("Grant.Issue", {
            "principal_id": reader["principal_id"], "actions": ["task.results.read"],
            "units": 1, "ttl_seconds": 120, "tool_targets": [upstream],
        })
        schedule(node, "Enabled")
        done = node.wait(dependent)
        assert done["lifecycle"] == "Succeeded", done
        run = node.call("Run.Get", {"id": done["current_run_id"]})
        snapshot = node.get(run["execution_snapshot_ref"]["resource_id"])["value"]
        view = node.call("Task.Results", {"id": upstream}, reader["token"])
        reference, = view["result"]["artifact_refs"]
        assert snapshot["required_artifact_refs"] == [reference] and reference["digest"] == expected
        assert node.call("Artifact.Read", {"reference": reference}, reader["token"]) == result
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 45 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 44 pending dependency survived migration and new scoped read returned its original result")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
