"""Recover a genuinely published legacy backup without reissuing its capture."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Node, target


def verify(command):
    return json.loads(subprocess.check_output(command + ["verify"], timeout=20))


def main(previous, current):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "previous-format-backup-source"})
        old_source = node.state / "volumes" / volume["resource_id"] / "data" / "record"
        payload = b"old exact backup\x00content\xff"
        old_source.write_bytes(payload)
        database_path = node.state / "state.db"
        with closing(sqlite3.connect(database_path)) as database, database:
            database.execute("""
                CREATE TRIGGER lose_old_backup_receipt BEFORE INSERT ON versions
                WHEN NEW.kind='Backup' BEGIN SELECT RAISE(ABORT, 'fixture cut'); END
            """)
        request = node.prepare("backup.create", target(volume), submission_key="previous-format-backup")
        task_id = node.call("Task.Submit", request)["task_id"]
        pending = node.wait(task_id)
        assert (pending["lifecycle"], pending["outcome_knowledge"]) == ("Paused", "Unknown"), pending
        operation, = pending["operation_refs"]
        operation_id = operation["resource_id"]
        backup_id = f"backup-{operation_id}"
        original = node.state / "backups" / backup_id
        inode = original.stat().st_ino
        assert (original / "blob-0").read_bytes() == payload
        checkpoint = node.get(f"backup-publication-{operation_id}")
        assert checkpoint["value"]["receipt"]["id"] == backup_id, checkpoint
        assert "placement" not in checkpoint["value"] and "intent_ref" not in checkpoint["value"], checkpoint

        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 4, "ttl_seconds": 180,
            "actions": ["backup.inspect_publication", "task.submit", "task.read"], "tool_targets": [operation_id]})
        request_inspection = node.prepare("backup.inspect_publication", target(operation),
                                          delegation=grant["delegation_ref"]["resource_id"])
        inspection = node.wait(node.call("Task.Submit", request_inspection)["task_id"])
        assert inspection["lifecycle"] == "Succeeded", inspection
        evidence, = inspection["result"]["artifact_refs"]
        original_proof = node.call("Artifact.Read", {"reference": evidence})
        assert "publication" not in original_proof and original_proof["operation_ref"] == operation, original_proof

        old_daemon = node.daemon[:]
        node.stop(signal.SIGKILL)
        with closing(sqlite3.connect(database_path)) as database, database:
            database.execute("DROP TRIGGER lose_old_backup_receipt")
        source = verify(old_daemon)
        assert source["database_format"] == 122, source
        node.daemon[0] = str(Path(current).absolute() / "psiosd")
        before = verify(node.daemon)
        assert before["node_id"] == source["node_id"] and before["migration_required"], before
        node.start()
        assert node.wait(task_id) == pending
        assert node.get(f"backup-publication-{operation_id}") == checkpoint
        assert node.call("Artifact.Read", {"reference": evidence}) == original_proof
        reconciled = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": evidence})
        assert (reconciled["lifecycle"], reconciled["outcome_knowledge"]) == ("Succeeded", "Known"), reconciled
        settled = node.wait(task_id)
        receipt, = settled["result"]["artifact_refs"]
        catalog = node.call("Artifact.Read", {"reference": receipt})
        assert catalog["resource_id"] == backup_id, catalog
        assert original.stat().st_ino == inode and (original / "blob-0").read_bytes() == payload
        replay = node.call("Task.Submit", request)
        assert replay["deduplicated"] and replay["task_id"] == task_id, replay
        node.stop(signal.SIGKILL)
        after = verify(node.daemon)
        assert after["database_format"] == 123 and not after["migration_required"], after
        assert after["node_id"] == source["node_id"], after
        old = subprocess.run(old_daemon + ["serve"], capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        print("PASS format 122 backup checkpoint and artifact retain their original positive settlement")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin-dir", required=True, type=Path)
    parser.add_argument("--bin-dir", default=os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"), type=Path)
    args = parser.parse_args()
    main(args.previous_bin_dir, args.bin_dir)
