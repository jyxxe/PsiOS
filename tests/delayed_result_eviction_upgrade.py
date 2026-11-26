"""Migrate a real format-90 authority with existing results and manual eviction history."""
import argparse
from pathlib import Path
import signal
import subprocess
import time

from project_upgrade import verify
from support import Failure, Node

PREPARE = "Task.PrepareResultEvictionBatch"
APPLY = "Task.ApplyResultEvictionBatch"
SCHEDULE = "Task.ScheduleResultEvictionBatch"
GET = "Task.GetResultEvictionSchedule"


def observe(node):
    task_id = node.submit("system.status", {})
    task = node.wait(task_id)
    assert task["lifecycle"] == "Succeeded" and task["outcome_knowledge"] == "Known", task
    artifact, = task["result"]["artifact_refs"]
    return node.get(task_id)["reference"], artifact


def require_error(node, code, method, params):
    try:
        node.call(method, params)
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"{method} unexpectedly succeeded")


def upgrade(previous, current):
    node = Node(previous)
    previous_daemon = node.daemon.copy()
    try:
        old_task, old_artifact = observe(node)
        delayed_task, delayed_artifact = observe(node)
        surviving_task, surviving_artifact = observe(node)
        delayed_bytes = node.call("Artifact.Read", {"reference": delayed_artifact})
        surviving_bytes = node.call("Artifact.Read", {"reference": surviving_artifact})
        require_error(node, "UnsupportedCapability", SCHEDULE, {"plan_token": "unavailable"})
        plan = node.call(PREPARE, {"task_refs": [old_task], "reason": "original format 90 eviction"})
        token = {"plan_token": plan["plan_token"]}
        receipt = node.call(APPLY, token)
        require_error(node, "ResultEvicted", "Artifact.Read", {"reference": old_artifact})
        node.stop(signal.SIGKILL)

        old = verify(node)
        assert old["database_format"] == 90 and not old["migration_required"], old
        witness = (node.witness / "head").read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 90 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == delayed_bytes["authority_id"], before
        assert (node.witness / "head").read_bytes() == witness

        node.start()
        assert node.call(APPLY, token) == receipt
        require_error(node, "ResultEvicted", "Artifact.Read", {"reference": old_artifact})
        assert node.get(delayed_task["resource_id"])["reference"] == delayed_task
        assert node.call("Artifact.Read", {"reference": delayed_artifact}) == delayed_bytes
        plan = node.call(PREPARE, {
            "task_refs": [delayed_task], "reason": "exact pre-upgrade result", "delay_seconds": 1,
        })
        delayed_token = {"plan_token": plan["plan_token"]}
        pending = node.call(SCHEDULE, delayed_token)
        assert pending["schedule"]["lifecycle"] == "Scheduled", pending
        schedule_id = pending["reference"]["resource_id"]
        limit = time.monotonic() + 20
        result = pending
        while time.monotonic() < limit:
            result = node.call(GET, {"id": schedule_id})
            if result["schedule"]["lifecycle"] != "Scheduled":
                break
            time.sleep(0.05)
        assert result["schedule"]["lifecycle"] == "Evicted", result
        batch = node.call("Resource.Resolve", {"reference": result["schedule"]["batch_ref"]})["value"]
        assert batch["schedule_ref"] == pending["reference"], batch
        require_error(node, "ResultEvicted", "Artifact.Read", {"reference": delayed_artifact})
        assert node.get(surviving_task["resource_id"])["reference"] == surviving_task
        assert node.call("Artifact.Read", {"reference": surviving_artifact}) == surviving_bytes
        assert node.call(SCHEDULE, delayed_token) == result
        assert node.call(APPLY, token) == receipt

        node.stop()
        after = verify(node)
        assert after["database_format"] >= 91 and not after["migration_required"], after
        assert after["node_id"] == old["node_id"] and after["sqlite_integrity"] == "Verified", after
        obsolete = subprocess.run([*previous_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format-90 manual receipt and results survive format-91 migration and scheduled eviction")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
