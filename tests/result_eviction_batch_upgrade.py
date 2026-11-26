"""Reclaim format-85 Task results in a format-86 batch while retaining the old single receipt."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from support import Failure, Node, target
from test_task_result_eviction_batch import APPLY, PREPARE, Observation


def upgrade(previous, current):
    node = Node(previous)
    old_daemon = node.daemon.copy()

    def rejects(code, method, params):
        try:
            node.call(method, params)
        except Failure as error:
            assert error.code == code, error
        else:
            raise AssertionError(f"{method} unexpectedly succeeded")

    def observe():
        task_id = node.submit("system.status", {})
        state = node.wait(task_id)
        assert (state["lifecycle"], state["outcome_knowledge"]) == ("Succeeded", "Known"), state
        reference = node.get(task_id)["reference"]
        artifact, = state["result"]["artifact_refs"]
        query = {"reference": artifact}
        payload = node.call("Artifact.Read", query)
        spec = node.call("Artifact.Get", query)["resource"]["spec"]
        assert payload["authority_id"] and payload["kernel_release"], payload
        return Observation(reference, state, artifact, payload, int(spec["size_bytes"]))

    def cli(command, payload):
        result = subprocess.check_output([
            node.binaries / "psiosctl", "--socket", node.socket,
            "--token-file", node.witness / "owner.token", command,
        ], input=json.dumps(payload), text=True, timeout=20)
        return json.loads(result)

    def verify_receipt(original, receipt):
        data = receipt["eviction"]
        assert data["previous_task_ref"] == original.task and data["artifact_refs"] == [original.artifact], data
        assert data["content_bytes_removed"] == str(original.content_bytes), data
        assert node.get(original.task["resource_id"])["reference"] == data["task_ref"]
        assert node.call("Resource.Resolve", {"reference": original.task})["value"] == original.task_state
        info = node.call("Artifact.Get", {"reference": original.artifact})
        assert info["availability"] == "Evicted" and info["retention"] == receipt, info
        rejects("ResultEvicted", "Artifact.Read", {"reference": original.artifact})

    try:
        single, first, second = [observe() for _ in range(3)]
        selected = [first, second]
        total = sum(item.content_bytes for item in selected)
        request = {
            "task_refs": [item.task for item in selected],
            "reason": "reclaim both exact pre-upgrade results",
            "max_content_bytes": total,
        }
        rejects("UnsupportedCapability", PREPARE, request)
        rejects("UnsupportedCapability", APPLY, {"plan_token": "unavailable"})
        single_request = {**target(single.task), "reason": "pre-upgrade single-result eviction"}
        single_receipt = node.call("Task.EvictResults", single_request)
        verify_receipt(single, single_receipt)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 85 and not old["migration_required"], old
        witness = node.witness / "head"
        previous_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 85 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == first.payload["authority_id"], before
        assert witness.read_bytes() == previous_head
        node.start()

        assert node.call("Task.EvictResults", single_request) == single_receipt
        verify_receipt(single, single_receipt)
        for item in selected:
            assert node.get(item.task["resource_id"])["reference"] == item.task
            assert node.call("Artifact.Read", {"reference": item.artifact}) == item.payload
        plan = cli("task-result-eviction-plan", request)
        assert plan["content_bytes"] == total and plan["plan_token"], plan
        assert {item["task_ref"]["resource_id"]: item["content_bytes"] for item in plan["items"]} == {
            item.task["resource_id"]: item.content_bytes for item in selected
        }, plan
        receipt = cli("task-result-eviction-apply", plan)
        batch = receipt["batch"]
        assert batch["content_bytes_removed"] == str(total) and batch["reason"] == request["reason"], batch
        records = {}
        for ref in batch["eviction_refs"]:
            data = node.call("Resource.Resolve", {"reference": ref})["value"]
            records[data["previous_task_ref"]["resource_id"]] = {"reference": ref, "eviction": data}
        assert len(batch["eviction_refs"]) == len(records) == 2
        for item in selected:
            verify_receipt(item, records[item.task["resource_id"]])

        node.stop(signal.SIGKILL)
        node.start()
        assert cli("task-result-eviction-apply", plan) == receipt
        assert node.call("Task.EvictResults", single_request) == single_receipt
        verify_receipt(single, single_receipt)
        for item in selected:
            verify_receipt(item, records[item.task["resource_id"]])
        audit = node.call("Audit.List", {"limit": 128})
        assert [event["resource"] for event in audit if event["method"] == APPLY and event["decision"] == "Evicted"] == [
            receipt["reference"]["resource_id"],
        ], audit

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 86 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["sqlite_integrity"] == "Verified", report
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 85 Task results and single receipt support durable format 86 batch eviction")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
