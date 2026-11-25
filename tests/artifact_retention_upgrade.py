"""Evict a chunked observation produced by the actual format 30 binary."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node


def main(previous):
    n = Node(previous)
    try:
        submission = n.prepare("system.status", {}, "before-retention-upgrade")
        tid = n.call("Task.Submit", submission)["task_id"]
        task = n.wait(tid)
        reference, = task["result"]["artifact_refs"]
        body = n.call("Artifact.Read", {"reference": reference})
        current = Path("target/debug").absolute()
        n.stop(signal.SIGKILL)
        n.daemon[0] = str(current / "psiosd")
        before = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 30 and before["migration_required"], before

        n.start()
        assert n.call("Artifact.Read", {"reference": reference}) == body
        request = {"id": tid, "expected_resource_version": task["task_revision"], "reason": "expired"}
        receipt = n.call("Task.EvictResults", request)
        assert receipt["eviction"]["artifact_refs"] == [reference]
        assert n.call("Artifact.Get", {"reference": reference})["retention"] == receipt
        assert n.call("Task.Submit", submission)["deduplicated"]
        retained = n.call("Task.Get", {"id": tid})
        assert retained["lifecycle"] == "Succeeded" and retained["result"]["availability"] == "Evicted"
        try:
            n.call("Artifact.Read", {"reference": reference})
        except Failure as error:
            assert error.code == "ResultEvicted", error
        else:
            raise AssertionError("evicted result remained readable")

        n.stop()
        after = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert after["node_id"] == before["node_id"]
        assert after["database_format"] >= 31 and not after["migration_required"], after
        old = subprocess.run([str(n.binaries / "psiosd"), "--config", n.config, "serve"],
                             capture_output=True, timeout=15)
        assert old.returncode != 0 and b"SchemaVersionUnsupported" in old.stderr, old
        print("PASS format 30 chunked result migration, eviction, deduplication and downgrade refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
