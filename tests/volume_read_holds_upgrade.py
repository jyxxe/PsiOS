"""Requeue a live format-48 read and release its old exclusive volume hold."""
import argparse
import hashlib
import json
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Node, target
from test_volume_file_integrity import wait_for_open_file


def scheduling(node, mode):
    ref = node.call("Node.Get", {})["reference"]
    node.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})


def main(previous):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "legacy-live-reader"})
        path = node.state / "volumes" / volume["resource_id"] / "data" / "entry"
        length = 256 * 1024 * 1024
        with path.open("wb") as file:
            file.truncate(length)
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", "volume.verify_file"],
            "tool_targets": [volume["resource_id"]], "units": 4, "ttl_seconds": 180,
        })
        prepared = node.prepare("volume.verify_file", {"target": target(volume), "path": "entry", "max_bytes": length},
                                delegation=grant["delegation_ref"]["resource_id"])
        task = node.call("Task.Submit", prepared)["task_id"]
        wait_for_open_file(node, path)
        scheduling(node, "Cordoned")
        node.stop(signal.SIGKILL)
        with sqlite3.connect(node.state / "state.db") as db:
            original = db.execute("SELECT phase,operation FROM queue WHERE task=?", (task,)).fetchone()
            assert original[0] == "Dispatched", original
            assert db.execute("SELECT holder FROM holds WHERE volume=?", (volume["resource_id"],)).fetchone() == (original[1],)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 48 and before["migration_required"], before
        node.start()
        assert node.call("Task.Get", {"id": task})["lifecycle"] == "Queued"
        with sqlite3.connect(node.state / "state.db") as db:
            assert not db.execute("SELECT 1 FROM holds WHERE volume=?", (volume["resource_id"],)).fetchone()
            assert not db.execute("SELECT 1 FROM volume_read_holds WHERE volume=?", (volume["resource_id"],)).fetchone()
        scheduling(node, "Enabled")
        result = node.wait(task)
        assert result["lifecycle"] == "Succeeded", result
        proof = node.call("Artifact.Read", {"reference": result["result"]["artifact_refs"][0]})
        checksum = hashlib.sha256()
        chunk = bytes(1024 * 1024)
        for _ in range(length // len(chunk)):
            checksum.update(chunk)
        assert proof["content_digest"] == "sha256:" + checksum.hexdigest(), proof
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 49 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 48 old exclusive read hold released and original Task retried with verified data")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
