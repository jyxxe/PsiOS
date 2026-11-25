"""Resolve a real lost restore receipt that filled a pre-reserve format 45 queue."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, target
from test_recovery_admission import lose_restore_receipt


def main(previous):
    node = Node(previous)
    try:
        node.stop()
        settings = json.loads(node.config.read_text())
        settings["queue_limit"] = 1
        node.config.write_text(json.dumps(settings))
        node.start()
        volume = node.perform("volume.create", {"name": "before-recovery-lane"})
        data = node.state / "volumes" / volume["resource_id"] / "data"
        (data / "record").write_bytes(b"already copied")
        backup = node.perform("backup.create", target(volume))
        task, unresolved = lose_restore_receipt(node, backup)
        assert unresolved["outcome_knowledge"] == "Unknown", unresolved
        operation, = unresolved["operation_refs"]
        tool = "volume.inspect_publication"
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", tool],
            "units": 2, "ttl_seconds": 120, "tool_targets": [operation["resource_id"]],
        })
        request = node.prepare(tool, target(operation), delegation=grant["delegation_ref"]["resource_id"])
        try:
            node.call("Task.Submit", request)
        except Failure as refused:
            assert refused.code == "QueueFull", refused
        else:
            raise AssertionError("format 45 unexpectedly admitted work beyond its queue limit")
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 45 and before["migration_required"], before
        node.start()
        assert node.call("Task.Get", {"id": task}) == unresolved
        saved = node.call("Task.Submit", request)
        evidence, = node.wait(saved["task_id"])["result"]["artifact_refs"]
        known = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": evidence})
        assert known["outcome_knowledge"] == "Known", known
        settled = node.wait(task)
        assert settled["lifecycle"] == "Succeeded", settled
        assert node.call("Node.Get", {})["admission"]["ordinary"]["used"] == 0
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 46 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 45 saturated unknown restore recovered through the same proof request")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
