"""Verify backup manifest compatibility across two format 5 binaries and their successor."""
import argparse
import json
from pathlib import Path

from support import Node, target


def switch(node, binaries):
    node.stop()
    node.daemon[0] = str((binaries / "psiosd").resolve())
    node.start()


def exercise(previous, format5_writer, current):
    node = Node(previous)
    try:
        source = node.perform("volume.create", {"name": "records"})
        folder = node.state / "volumes" / source["resource_id"] / "data"
        (folder / "entry").write_bytes(b"previous volume")
        older = node.perform("backup.create", target(source))
        def manifest(ref):
            path = node.state / "backups" / ref["resource_id"] / "manifest.json"
            return json.loads(path.read_bytes())
        assert "mtime_unix_v1" not in manifest(older)
        def check_restore(ref, verify_activation=False):
            grant = node.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=[ref["resource_id"]],
                actions=["task.submit", "task.read", "backup.verify"], units=1, ttl_seconds=120))
            request = node.prepare("backup.verify", target(ref), delegation=grant["delegation_ref"]["resource_id"])
            task = node.wait(node.call("Task.Submit", request)["task_id"])
            assert task["lifecycle"] == "Succeeded", task
            restored = node.perform("backup.restore", target(ref))
            data = node.state / "volumes" / restored["resource_id"] / "data"
            assert (data / "entry").read_bytes() == b"previous volume"
            if verify_activation:
                active = node.perform("volume.activate", target(restored))
                spec = node.get(active["resource_id"])["resource"]["spec"]
                assert spec["restore_proof"]["includes_mtime"] == ("mtime_unix_v1" in manifest(ref))
                assert spec["restore_verification_ref"]["resource_id"]
            return data
        switch(node, format5_writer)
        check_restore(older)
        newer = node.perform("backup.create", target(source))
        assert "mtime_unix_v1" in manifest(newer)
        data = check_restore(newer)
        assert (data / "entry").stat().st_mtime_ns == (folder / "entry").stat().st_mtime_ns
        switch(node, previous)
        check_restore(newer)
        switch(node, current)
        check_restore(older, verify_activation=True)
        data = check_restore(newer, verify_activation=True)
        assert (data / "entry").stat().st_mtime_ns == (folder / "entry").stat().st_mtime_ns
        print("PASS both format 5 backup-manifest directions, restore and verified activation after upgrade")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--format5-writer-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.format5_writer_bin, args.current_bin)
