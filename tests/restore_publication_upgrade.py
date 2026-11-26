"""Exercise restore publication compatibility with real format 41 or 75 binaries."""
import argparse
from contextlib import closing
import json
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Node, target


INSPECT = "volume.inspect_publication"


def inspection_grant(node, operation):
    return node.call("Grant.Issue", {
        "principal_id": "local-owner", "units": 2, "ttl_seconds": 180,
        "actions": ["task.submit", "task.read", INSPECT],
        "tool_targets": [operation["resource_id"]],
    })["delegation_ref"]["resource_id"]


def inspect(node, operation, delegation):
    request = node.prepare(INSPECT, target(operation), delegation=delegation)
    return node.wait(node.call("Task.Submit", request)["task_id"])


def observation(node, task):
    assert task["lifecycle"] == "Succeeded", task
    result, = task["result"]["artifact_refs"]
    return result, node.call("Artifact.Read", {"reference": result})


def main(previous, current, source_format):
    node = Node(previous)
    try:
        origin = node.perform("volume.create", {"name": "before-restore-upgrade"})
        content = node.state / "volumes" / origin["resource_id"] / "data" / "content"
        text = f"retained format {source_format} backup"
        content.write_text(text)
        backup = node.perform("backup.create", target(origin))
        old_source = node.perform("backup.create", target(origin))
        with closing(sqlite3.connect(node.state / "state.db", timeout=10)) as database, database:
            database.execute("""CREATE TRIGGER lose_old_restore BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.spec->>'storage_restore_phase'='AwaitingActivation'
                BEGIN SELECT RAISE(ABORT, 'lost old restore receipt'); END""")
        old_task = node.submit("backup.restore", target(old_source))
        unknown = node.wait(old_task)
        assert unknown["lifecycle"] == "Paused" and unknown["outcome_knowledge"] == "Unknown", unknown
        old_op = unknown["operation_refs"][0]
        if source_format == 75:
            checkpoint = node.get(f"restore-publication-{old_op['resource_id']}")
            placement = checkpoint["value"]["placement"]
            assert "stage_name" not in placement, checkpoint
            volume_id = f"volume-{old_op['resource_id']}"
            published = node.state / "volumes" / volume_id
            published_file = published / "data" / "content"
            assert published_file.read_text() == text
            staged_identity = (published.stat().st_dev, published.stat().st_ino)
            assert placement["directory"] == list(map(str, staged_identity)), checkpoint
            delegation = inspection_grant(node, old_op)
            old_evidence, legacy = observation(node, inspect(node, old_op, delegation))
            assert legacy["operation_ref"] == old_op and legacy["checkpoint_ref"] == checkpoint["reference"], legacy
            assert "publication" not in legacy, legacy
        node.stop(signal.SIGKILL)
        with closing(sqlite3.connect(node.state / "state.db", timeout=10)) as database, database:
            database.execute("DROP TRIGGER lose_old_restore")
        node.daemon[0] = str((current / "psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == source_format and before["migration_required"], before
        node.start()
        capability = node.get("tool-volume.inspect_publication")["value"]
        assert capability["actions"] == [INSPECT], capability
        assert node.wait(old_task) == unknown
        if source_format == 41:
            absent = inspect(node, old_op, inspection_grant(node, old_op))
            assert absent["conditions"][0]["type"] == "RecoveryRequired", absent
            assert node.wait(old_task) == unknown
        else:
            assert node.get(checkpoint["reference"]["resource_id"]) == checkpoint
            _, fresh = observation(node, inspect(node, old_op, delegation))
            assert (fresh["publication"], fresh["operation_ref"], fresh["checkpoint_ref"]) == (
                "Published", old_op, checkpoint["reference"],
            ), fresh
            # A genuinely pre-upgrade observation can still settle its exact pre-upgrade checkpoint.
            settled = node.call("Operation.Reconcile", {**target(old_op), "evidence_ref": old_evidence})
            assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
            _, exact = observation(node, node.wait(old_task))
            assert exact["resource_id"] == volume_id, exact
            assert node.get(volume_id)["resource"]["spec"]["restored_from"] == old_source
            assert (published.stat().st_dev, published.stat().st_ino) == staged_identity
            assert published_file.read_text() == text

        restored = node.perform("backup.restore", target(backup))
        data = node.state / "volumes" / restored["resource_id"] / "data" / "content"
        assert data.read_text() == text
        spec = node.get(restored["resource_id"])["resource"]["spec"]
        assert spec["restored_from"] == backup and spec["storage_restore_phase"] == "AwaitingActivation", spec
        node.perform("volume.activate", target(restored))
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] > source_format and not after["migration_required"], after
        assert before["node_id"] == after["node_id"]
        if source_format == 41:
            print("PASS format 41 preserved an ambiguous restore and activated a new restore of a retained backup")
        else:
            print("PASS format 75 checkpoint and observation cataloged the original published volume after migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    parser.add_argument("--source-format", choices=(41, 75), default=41, type=int)
    args = parser.parse_args()
    main(args.previous_bin, args.current_bin, args.source_format)
