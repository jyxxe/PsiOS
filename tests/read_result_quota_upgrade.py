"""Account for only retained content and preserve pre-existing work across migration."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, target
from test_read_result_quota import RESERVATION


def scheduling(node, mode):
    ref = node.call("Node.Get", {})["reference"]
    node.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})


def main(previous):
    node = Node(previous)
    try:
        kept = node.submit("system.status", {})
        original, = node.wait(kept)["result"]["artifact_refs"]
        original_body = node.call("Artifact.Read", {"reference": original})
        size = int(node.call("Artifact.Get", {"reference": original})["resource"]["spec"]["size_bytes"])
        gone = node.submit("system.status", {})
        node.wait(gone)
        node.call("Task.EvictResults", {**target(node.get(gone)["reference"]), "reason": "evicted before migration"})
        scheduling(node, "Cordoned")
        pending = node.submit("system.status", {})
        node.stop(signal.SIGKILL)

        config = json.loads(node.config.read_text())
        config["read_result_bytes_per_principal"] = RESERVATION
        node.config.write_text(json.dumps(config))
        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 50 and before["migration_required"], before
        node.start()
        prior = node.call("Task.Admission", {})["read_results"]
        assert int(prior["retained_bytes"]) == size and int(prior["reserved_bytes"]) == RESERVATION, prior
        try:
            node.submit("system.status", {})
        except Failure as denied:
            assert denied.code == "QuotaExceeded", denied
        else:
            raise AssertionError("existing results and pending work were omitted from the quota")
        scheduling(node, "Enabled")
        completed = node.wait(pending)
        assert completed["lifecycle"] == "Succeeded", completed
        retained, = completed["result"]["artifact_refs"]
        new_size = int(node.call("Artifact.Get", {"reference": retained})["resource"]["spec"]["size_bytes"])
        current = node.call("Task.Admission", {})["read_results"]
        assert int(current["retained_bytes"]) == size + new_size and current["reserved_bytes"] == "0", current
        assert node.call("Artifact.Read", {"reference": original}) == original_body
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 51 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 50 accounted for retained and pending read bytes, excluding an evicted result")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
