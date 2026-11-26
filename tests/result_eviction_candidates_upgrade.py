"""Discover available Tasks created by a real format-86 authority after upgrading to 87."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from support import Failure, Node, target
from test_result_eviction_candidates import CANDIDATES
from test_schedule import timestamp
from test_task_result_eviction_batch import APPLY, PREPARE, Observation


def upgrade(previous, current):
    node = Node(previous)
    old_daemon = node.daemon.copy()

    def rejects(code, method, params, token=None):
        try:
            node.call(method, params, token)
        except Failure as error:
            assert error.code == code, error
        else:
            raise AssertionError(f"{method} unexpectedly succeeded")

    def observe(token=None, delegation="owner-delegation"):
        request = node.prepare("system.status", {}, token=token, delegation=delegation)
        task_id = node.call("Task.Submit", request, token)["task_id"]
        task = node.wait(task_id)
        assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
        artifact, = task["result"]["artifact_refs"]
        query = {"reference": artifact}
        payload = node.call("Artifact.Read", query, token)
        spec = node.call("Artifact.Get", query)["resource"]["spec"]
        return Observation(node.get(task_id)["reference"], task, artifact, payload, int(spec["size_bytes"]))

    def cli(command, payload):
        result = subprocess.check_output([
            node.binaries / "psiosctl", "--socket", node.socket,
            "--token-file", node.witness / "owner.token", command,
        ], input=json.dumps(payload), text=True, timeout=20)
        return json.loads(result)

    def candidate(observation):
        return {"task_ref": observation.task, "content_bytes": observation.content_bytes}

    def verify_eviction(receipt, observations):
        batch = receipt["batch"]
        assert batch["content_bytes_removed"] == str(sum(item.content_bytes for item in observations)), batch
        records = {}
        for ref in batch["eviction_refs"]:
            entry = node.call("Resource.Resolve", {"reference": ref})["value"]
            records[entry["previous_task_ref"]["resource_id"]] = {"reference": ref, "eviction": entry}
        assert len(batch["eviction_refs"]) == len(records) == len(observations), batch
        for item in observations:
            record = records[item.task["resource_id"]]
            data = record["eviction"]
            assert data["previous_task_ref"] == item.task and data["artifact_refs"] == [item.artifact], data
            assert data["content_bytes_removed"] == str(item.content_bytes), data
            info = node.call("Artifact.Get", {"reference": item.artifact})
            assert info["availability"] == "Evicted" and info["retention"] == record, info
            rejects("ResultEvicted", "Artifact.Read", {"reference": item.artifact})

    try:
        agent = node.call("Agent.Register", {"name": "format86-result-selection"})
        principal, token = agent["principal_id"], agent["token"]
        grant = node.call("Grant.Issue", {
            "principal_id": principal, "actions": ["system.status", "task.read", "task.submit"],
            "tool_targets": ["profile-local"], "units": 8, "ttl_seconds": 300,
        })["delegation_ref"]["resource_id"]
        retired = [observe(token, grant)]
        outside = observe()
        retired.append(observe(token, grant))
        first, second = [observe(token, grant) for _ in range(2)]
        archived = node.call("Task.Archive", target(second.task), token)

        query = {"updated_before": timestamp(180), "principal_id": principal}
        rejects("UnsupportedCapability", CANDIDATES, query)
        old_plan = node.call(PREPARE, {
            "task_refs": [item.task for item in retired], "reason": "pre-upgrade result eviction",
        })
        old_receipt = node.call(APPLY, {"plan_token": old_plan["plan_token"]})
        verify_eviction(old_receipt, retired)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 86 and not old["migration_required"], old
        witness = node.witness / "head"
        old_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 86 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == first.payload["authority_id"], before
        assert witness.read_bytes() == old_head
        node.start()

        assert node.call("Principal.WhoAmI", {}, token)["principal_id"] == principal
        for item in (first, second, outside):
            assert node.get(item.task["resource_id"])["reference"] == item.task
            assert node.call("Artifact.Read", {"reference": item.artifact}) == item.payload
        assert node.call("Resource.Resolve", {"reference": archived})["value"]["task_ref"] == second.task
        assert node.call(APPLY, {"plan_token": old_plan["plan_token"]}) == old_receipt
        verify_eviction(old_receipt, retired)
        rejects("PermissionDenied", CANDIDATES, query, token)
        page = cli("task-result-eviction-candidates", {**query, "limit": 1})
        assert page["items"] == [candidate(first)] and page["next_cursor"], page
        assert page["skipped"]["changed"] == len(retired), page

        selected = page["items"][0]
        plan = cli("task-result-eviction-plan", {
            "task_refs": [selected["task_ref"]], "max_content_bytes": selected["content_bytes"],
            "reason": "reclaim one exact indexed Task result",
        })
        receipt = cli("task-result-eviction-apply", plan)
        verify_eviction(receipt, [first])
        node.stop(signal.SIGKILL)
        node.start()

        continued = cli("task-result-eviction-candidates", {**query, "cursor": page["next_cursor"]})
        assert continued["items"] == [candidate(second)] and continued["next_cursor"] is None, continued
        fresh = cli("task-result-eviction-candidates", query)
        assert fresh["items"] == [candidate(second)], fresh
        assert node.call(CANDIDATES, {**query, "principal_id": "local-owner"})["items"] == [candidate(outside)]
        assert node.call("Artifact.Read", {"reference": second.artifact}, token) == second.payload
        assert cli("task-result-eviction-apply", plan) == receipt
        assert node.call(APPLY, {"plan_token": old_plan["plan_token"]}) == old_receipt
        verify_eviction(old_receipt, retired)
        verify_eviction(receipt, [first])

        node.stop()
        result = verify(node)
        assert result["database_format"] >= 87 and not result["migration_required"], result
        assert result["node_id"] == old["node_id"] and result["sqlite_integrity"] == "Verified", result
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format-86 results and receipt survive candidate-index migration, scoped selection, and restart")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
