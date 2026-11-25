"""Use a result produced by the actual pre-dependencies authority as a pinned input."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key, target


def main(previous):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "legacy-result"})
        file = node.state / "volumes" / volume["resource_id"] / "data" / "content"
        file.write_text("already verified in format 40")
        producer = node.submit("volume.read", {**target(volume), "name": "content"})
        completed = node.wait(producer)
        assert completed["lifecycle"] == "Succeeded", completed
        original = completed["result"]["artifact_refs"][0]
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 40 and before["migration_required"], before
        node.start()
        assert node.wait(producer) == completed
        request = node.call("Task.Prepare", {
            "key": key(), "tool": "system.status", "input": {}, "delegation_id": "owner-delegation",
            "requires": [{"task_id": producer, "digest": original["digest"]}],
        })
        dependent = node.call("Task.Submit", request)["task_id"]
        final = node.wait(dependent)
        assert final["lifecycle"] == "Succeeded", final
        run = node.call("Run.Get", {"id": final["current_run_id"]})
        snapshot = node.get(run["execution_snapshot_ref"]["resource_id"])["value"]
        assert snapshot["required_artifact_refs"] == [original], snapshot
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 41 and not after["migration_required"], after
        assert before["node_id"] == after["node_id"]
        print("PASS format 40 output survived upgrade and was verified as a pinned dependency")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
