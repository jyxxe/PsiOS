"""Discover original format-87 assignments through the format-88 CLI and durable tool."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from support import Failure, Node, key, target
from test_agent_assigned_work import ASSIGNED, DISCOVER, STATUS


def upgrade(previous, current):
    node = Node(previous)
    old_daemon = node.daemon.copy()
    capability_id = f"tool-{DISCOVER}"

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

    try:
        contributor = node.call("Agent.Register", {"name": "format87-assigned-contributor"})
        other = node.call("Agent.Register", {"name": "format87-other-assignee"})
        principal, token = contributor["principal_id"], contributor["token"]
        project = node.call("Project.Create", {"key": key(), "title": "Keep the original assigned work"})["reference"]
        project_id = project["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        for actor in (contributor, other):
            project = node.call("Project.SetMember", {
                **target(project), "principal_id": actor["principal_id"], "role": "Contributor",
            })["reference"]
            grant(actor["principal_id"], ["project.read", "project.contribute"], [project_id])
        roster = node.call("Project.Get", {"id": project_id}, token)
        profile = node.get("profile-local")["reference"]
        request = {
            "project_id": project_id, "expected_project_version": project["version"],
            "assignee_principal_id": principal,
            "criterion": {"kind": "TaskResult", "tool": STATUS, "target_ref": profile},
        }
        own = node.call("WorkItem.Create", {**request, "key": key(), "title": "Observe the local host"})
        moved = node.call("WorkItem.Create", {**request, "key": key(), "title": "Record a reassigned observation"})
        outside = node.call("WorkItem.Reassign", {
            **target(moved["reference"]), "assignee_principal_id": other["principal_id"],
        })
        own_query = {"id": own["reference"]["resource_id"]}
        outside_query = {"id": outside["reference"]["resource_id"]}
        assert own["value"]["lifecycle"] == outside["value"]["lifecycle"] == "Ready"
        assert outside["reference"] != moved["reference"]

        execution = grant(principal, ["task.submit", "task.read", "resource.read", STATUS], [profile["resource_id"]])
        status_request = node.prepare(STATUS, {}, token=token, delegation=execution)
        original_id = node.call("Task.Submit", status_request, token)["task_id"]
        original = node.wait(original_id)
        assert (original["lifecycle"], original["outcome_knowledge"]) == ("Succeeded", "Known"), original
        original_task = node.get(original_id)["reference"]
        artifact, = original["result"]["artifact_refs"]
        payload = node.call("Artifact.Read", {"reference": artifact}, token)
        query = {"project_id": project_id, "expected_project_version": project["version"]}
        prepare = {"tool": DISCOVER, "input": query, "key": key(), "delegation_id": execution}
        rejects("UnsupportedCapability", ASSIGNED, query, token)
        rejects("UnsupportedCapability", "Task.Prepare", prepare, token)
        rejects("NotFound", "Resource.Get", {"id": capability_id})

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 87 and not old["migration_required"], old
        witness = node.witness / "head"
        previous_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        preview = verify(node)
        assert preview["database_format"] == 87 and preview["migration_required"], preview
        assert preview["node_id"] == old["node_id"] == payload["authority_id"], preview
        assert witness.read_bytes() == previous_head
        node.start()

        assert node.call("Principal.WhoAmI", {}, token)["principal_id"] == principal
        assert node.call("Project.Get", {"id": project_id}, token) == roster
        assert node.call("WorkItem.Get", own_query) == own
        assert node.call("WorkItem.Get", outside_query) == outside
        assert node.get(original_id)["reference"] == original_task
        assert node.call("Artifact.Read", {"reference": artifact}, token) == payload
        token_file = node.base / "format87-contributor.token"
        with os.fdopen(os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            stream.write(token)
        listed = json.loads(subprocess.check_output([
            node.binaries / "psiosctl", "--socket", node.socket, "--token-file", token_file,
            "work-item", "assigned",
        ], input=json.dumps(query), text=True, timeout=20))
        assert listed == node.call(ASSIGNED, query, token), listed
        assert [entry["reference"] for entry in listed["items"]] == [own["reference"]], listed
        assert listed["next_after_id"] is None
        assert [entry["reference"] for entry in node.call(ASSIGNED, query, other["token"])["items"]] == [
            outside["reference"],
        ]
        rejects("PermissionDenied", "Task.Prepare", prepare, token)

        capability = node.get(capability_id)
        assert capability["value"]["actions"] == [DISCOVER], capability
        discovery = grant(principal, [DISCOVER, "task.submit", "task.read"], [project_id])
        submission = node.prepare(DISCOVER, query, token=token, delegation=discovery)
        accepted = node.call("Task.Submit", submission, token)
        task_id = accepted["task_id"]
        settled = node.wait(task_id)
        assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
        operation, = settled["operation_refs"]
        plan_ref = node.call("Resource.Resolve", {"reference": operation})["value"]["plan_ref"]
        plan = node.call("Resource.Resolve", {"reference": plan_ref})["value"]
        assert plan["runtime_refs"] == [capability["reference"]], plan
        assert [effect["resource_ref"] for effect in plan["allowed_effects"]] == [project], plan
        assert [condition["resource_ref"] for condition in plan["preconditions"]] == [project], plan
        page_ref, = settled["result"]["artifact_refs"]
        assert node.call("Artifact.Read", {"reference": page_ref}, token) == listed
        rejects("PermissionDenied", "Artifact.Read", {"reference": page_ref}, other["token"])

        node.stop(signal.SIGKILL)
        node.start()
        repeated = node.call("Task.Submit", submission, token)
        assert repeated["deduplicated"] and repeated["task_id"] == task_id, repeated
        assert node.call("Task.Get", {"id": task_id}, token) == settled
        assert node.call("Artifact.Read", {"reference": page_ref}, token) == listed
        assert node.call("WorkItem.Get", own_query) == own and node.call("WorkItem.Get", outside_query) == outside
        assert node.call("Artifact.Read", {"reference": artifact}, token) == payload
        assert node.call("Task.Get", {"id": original_id}, token) == original
        assert {ref["resource_id"] for ref in node.call("Resource.List", {"kind": "Task"})["items"]} == {
            original_id, task_id,
        }

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 88 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["sqlite_integrity"] == "Verified", report
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format-87 WorkItems and original result survive newly scoped format-88 discovery and replay")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
