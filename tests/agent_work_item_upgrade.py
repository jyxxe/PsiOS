"""Use a real format-81 Agent's Task result through the format-82 evidence tool."""
import argparse
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from support import Failure, Node, key, target


TOOL = "project.submit_evidence"
CAPABILITY = f"tool-{TOOL}"


def upgrade(previous, current):
    node = Node(previous)
    old_daemon = node.daemon.copy()

    def grant(principal, actions, targets):
        return node.call("Grant.Issue", {
            "principal_id": principal, "actions": actions, "tool_targets": targets,
            "units": 4, "ttl_seconds": 300,
        })["delegation_ref"]["resource_id"]

    def rejects(code, method, params, token=None):
        try:
            node.call(method, params, token)
        except Failure as error:
            assert error.code == code, error
        else:
            raise AssertionError(f"{method} unexpectedly succeeded")

    def discovered(token):
        return {entry["capability_id"]: entry for entry in node.call("Capability.Discover", {}, token)}

    try:
        agent = node.call("Agent.Register", {"name": "format81-evidence-contributor"})
        principal, token = agent["principal_id"], agent["token"]
        project = node.call("Project.Create", {"key": key(), "title": "Keep the pre-upgrade status"})
        project_id = project["reference"]["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        membership = node.call("Project.SetMember", {
            **target(project["reference"]), "principal_id": principal, "role": "Contributor",
        })
        grant(principal, ["project.read", "project.contribute"], [project_id])
        roster = node.call("Project.Get", {"id": project_id}, token)
        profile = node.get("profile-local")["reference"]
        item = node.call("WorkItem.Create", {
            "project_id": project_id, "expected_project_version": membership["reference"]["version"],
            "key": key(), "title": "Attach the observed host status", "assignee_principal_id": principal,
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        })["reference"]
        where = {"id": item["resource_id"]}
        assigned = node.call("WorkItem.Get", where, token)
        assert assigned["value"]["lifecycle"] == "Ready" and not assigned["value"]["has_evidence"], assigned

        old_execution = grant(principal, ["task.submit", "task.read", "system.status"], [profile["resource_id"]])
        request = node.prepare("system.status", {}, token=token, delegation=old_execution)
        task_id = node.call("Task.Submit", request, token)["task_id"]
        original = node.wait(task_id)
        assert (original["lifecycle"], original["outcome_knowledge"]) == ("Succeeded", "Known"), original
        task = node.get(task_id)["reference"]
        artifact, = original["result"]["artifact_refs"]
        output = {"reference": artifact}
        payload = node.call("Artifact.Read", output, token)
        status_capability = node.get("tool-system.status")["reference"]

        inputs = {**target(item), "task_ref": task, "artifact_ref": artifact}
        prepare = {"tool": TOOL, "input": inputs, "key": key(), "delegation_id": old_execution}
        assert TOOL not in discovered(token)
        rejects("NotFound", "Resource.Get", {"id": CAPABILITY})
        rejects("UnsupportedCapability", "Task.Prepare", prepare, token)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 81 and not old["migration_required"], old
        witness = node.witness / "head"
        original_witness = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 81 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == payload["authority_id"], before
        assert witness.read_bytes() == original_witness
        node.start()

        assert node.call("Principal.WhoAmI", {}, token)["principal_id"] == principal
        assert node.call("Project.Get", {"id": project_id}, token) == roster
        assert node.call("WorkItem.Get", where, token) == assigned
        assert node.call("Task.Get", {"id": task_id}, token) == original
        assert node.get(task_id)["reference"] == task
        assert node.call("Artifact.Read", output, token) == payload
        assert node.get("tool-system.status")["reference"] == status_capability
        capability = node.get(CAPABILITY)
        assert capability["value"]["actions"] == [TOOL] and capability["value"]["availability"] == "Ready", capability
        assert TOOL not in discovered(token)
        rejects("PermissionDenied", "Task.Prepare", prepare, token)

        execution = grant(principal, ["task.submit", "task.read", TOOL], [item["resource_id"]])
        assert discovered(token)[TOOL] == capability["value"]
        submission = node.prepare(TOOL, inputs, token=token, delegation=execution)
        accepted = node.call("Task.Submit", submission, token)
        receipt_id = accepted["task_id"]
        assert receipt_id != task_id, accepted
        settled = node.wait(receipt_id)
        assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
        operation, = settled["operation_refs"]
        op = node.call("Resource.Resolve", {"reference": operation})["value"]
        plan = node.call("Resource.Resolve", {"reference": op["plan_ref"]})["value"]
        assert plan["runtime_refs"] == [capability["reference"]], plan
        assert [effect["resource_ref"] for effect in plan["allowed_effects"]] == [item], plan
        receipt_ref, = settled["result"]["artifact_refs"]
        receipt = node.call("Artifact.Read", {"reference": receipt_ref}, token)
        attached = node.call("WorkItem.Get", where, token)
        evidence = attached["value"]["evidence"]
        assert receipt == {"work_item_ref": attached["reference"], "lifecycle": "InReview"}, receipt
        assert (evidence["task_ref"], evidence["artifact_ref"], evidence["submitted_by"]) == (
            task, artifact, principal,
        ), evidence

        node.stop(signal.SIGKILL)
        node.start()
        replay = node.call("Task.Submit", submission, token)
        assert replay["deduplicated"] and replay["task_id"] == receipt_id, replay
        assert node.call("WorkItem.Get", where, token) == attached
        assert node.call("Artifact.Read", {"reference": receipt_ref}, token) == receipt
        assert node.call("Artifact.Read", output, token) == payload
        assert node.get(CAPABILITY)["reference"] == capability["reference"]
        assert {ref["resource_id"] for ref in node.call("Resource.List", {"kind": "Task"})["items"]} == {
            task_id, receipt_id,
        }
        rejects("Conflict", "Task.EvictResults", {**target(task), "reason": "try to release linked evidence"})

        node.stop()
        after = verify(node)
        assert after["database_format"] >= 82 and not after["migration_required"], after
        assert after["node_id"] == old["node_id"] and after["sqlite_integrity"] == "Verified", after
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 81 WorkItem and Artifact survive migration, scoped evidence submission, and crash replay")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
