"""Upgrade a real format 38 authority while a volume deletion awaits approval."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Node, target


def main(previous):
    node = Node(previous)
    try:
        volume = node.perform("volume.create", {"name": "retirement-upgrade"})
        directory = node.state / "volumes" / volume["resource_id"]
        data = directory / "data" / "payload"
        data.write_text("kept through migration")
        task_id = node.submit("volume.delete", target(volume))
        pending = node.wait(task_id)
        assert pending["lifecycle"] == "WaitingApproval", pending
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        original = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert original["database_format"] == 38 and original["migration_required"], original
        node.start()
        assert node.wait(task_id) == pending
        assert data.read_text() == "kept through migration"
        node.approve(task_id)
        final = node.wait(task_id)
        assert final["lifecycle"] == "Succeeded" and final["outcome_knowledge"] == "Known", final
        receipt = node.call("Artifact.Read", {"reference": final["result"]["artifact_refs"][0]})
        retirement = receipt["receipt"]["retirement_ref"]["resource_id"]
        deadline = time.monotonic() + 10
        while node.get(retirement)["value"]["lifecycle"] != "Reclaimed":
            assert time.monotonic() < deadline, "upgraded authority did not reclaim a deletion"
            time.sleep(0.05)
        assert node.get(volume["resource_id"])["value"]["lifecycle"] == "Deleted"
        assert not directory.exists()
        node.stop()
        migrated = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert migrated["database_format"] >= 39 and not migrated["migration_required"], migrated
        assert migrated["node_id"] == original["node_id"]
        print("PASS format 38 retained the waiting approval and removed the approved volume after upgrade")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
