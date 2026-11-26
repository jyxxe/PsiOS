"""Reprioritize an exact Ready WorkItem created by a real format-88 authority."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from support import Failure, Node, key, target
from test_agent_assigned_work import ASSIGNED, DISCOVER, STATUS
from test_work_item_priority import REPRIORITIZE


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

    def discover(inputs, token, execution):
        request = node.prepare(DISCOVER, inputs, token=token, delegation=execution)
        task_id = node.call("Task.Submit", request, token)["task_id"]
        result = node.wait(task_id)
        assert (result["lifecycle"], result["outcome_knowledge"]) == ("Succeeded", "Known"), result
        artifact, = result["result"]["artifact_refs"]
        return node.get(task_id)["reference"], artifact, node.call("Artifact.Read", {"reference": artifact}, token)

    try:
        agent = node.call("Agent.Register", {"name": "format88-priority-contributor"})
        principal, token = agent["principal_id"], agent["token"]
        project = node.call("Project.Create", {"key": key(), "title": "Preserve a legacy priority"})["reference"]
        project_id = project["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        project = node.call("Project.SetMember", {
            **target(project), "principal_id": principal, "role": "Contributor",
        })["reference"]
        grant(principal, ["project.read", "project.contribute"], [project_id])
        roster = node.call("Project.Get", {"id": project_id}, token)
        profile = node.get("profile-local")["reference"]
        creation = {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Elevate this pre-upgrade observation", "assignee_principal_id": principal,
            "criterion": {"kind": "TaskResult", "tool": STATUS, "target_ref": profile},
        }
        original = node.call("WorkItem.Create", creation)
        peer = node.call("WorkItem.Create", {**creation, "key": key(), "title": "Leave this observation normal"})
        original_ref = original["reference"]
        current_query = {"id": original_ref["resource_id"]}
        former_query = {"reference": original_ref}
        assert original["value"]["lifecycle"] == "Ready" and "priority" not in original["value"], original

        query = {"project_id": project_id, "expected_project_version": project["version"]}
        only_high = {**query, "min_priority": "High"}
        high = {**target(original_ref), "priority": "High"}
        rejects("UnsupportedCapability", REPRIORITIZE, high)
        rejects("InvalidArgument", "WorkItem.Create", {**creation, "key": key(), "priority": "High"})
        rejects("InvalidArgument", ASSIGNED, only_high, token)
        execution = grant(principal, ["task.submit", "task.read", "resource.read", DISCOVER], [project_id])
        original_view = node.call("WorkItem.RevisionGet", former_query, token)
        old_task, old_artifact, old_page = discover(query, token, execution)
        references = sorted([original_ref, peer["reference"]], key=lambda ref: ref["resource_id"])
        assert [entry["reference"] for entry in old_page["items"]] == references, old_page

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 88 and not old["migration_required"], old
        witness = node.witness / "head"
        old_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        preview = verify(node)
        assert preview["database_format"] == 88 and preview["migration_required"], preview
        assert preview["node_id"] == old["node_id"] and witness.read_bytes() == old_head, preview
        node.start()

        assert node.call("Principal.WhoAmI", {}, token)["principal_id"] == principal
        assert node.call("Project.Get", {"id": project_id}, token) == roster
        assert node.call("WorkItem.Get", current_query) == original
        assert node.get(old_task["resource_id"])["reference"] == old_task
        assert node.call("Artifact.Read", {"reference": old_artifact}, token) == old_page
        assert node.call(ASSIGNED, {**query, "min_priority": "Normal"}, token) == old_page
        assert node.call(ASSIGNED, only_high, token)["items"] == []
        assert node.call("WorkItem.Create", {**creation, "priority": "Normal"}) == original

        owner_token = node.witness / "owner.token"
        cli = ("work-item", "reprioritize", original_ref["resource_id"], "--revision", original_ref["version"])
        assert node.cli(owner_token, *cli, "normal") == original
        history = node.call("WorkItem.History", current_query, token)["items"]
        assert [entry["reference"] for entry in history] == [original_ref], history
        rejects("PermissionDenied", REPRIORITIZE, high, token)
        elevated = node.cli(owner_token, *cli, "high")
        assert elevated["value"]["priority"] == "High", elevated
        assert elevated["value"]["priority_change"]["previous_priority"] == "Normal", elevated
        assert elevated["reference"]["resource_id"] == original_ref["resource_id"], elevated
        assert elevated["reference"] != original_ref
        rejects("Conflict", REPRIORITIZE, high)

        token_file = node.base / "format88-assignee.token"
        with os.fdopen(os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            stream.write(token)
        page = json.loads(subprocess.check_output([
            node.binaries / "psiosctl", "--socket", node.socket, "--token-file", token_file,
            "work-item", "assigned",
        ], input=json.dumps(only_high), text=True, timeout=20))
        chosen, = page["items"]
        assert chosen["reference"] == elevated["reference"] and chosen["value"]["priority"] == "High", chosen
        assert page["next_after_id"] is None
        new_task, new_artifact, result = discover(only_high, token, execution)
        assert result == page and new_task != old_task
        assert node.call("WorkItem.RevisionGet", former_query, token) == original_view
        assert node.call("WorkItem.Create", creation) == elevated

        node.stop(signal.SIGKILL)
        node.start()
        assert node.call(ASSIGNED, only_high, token) == page
        assert node.call("WorkItem.Get", current_query) == elevated
        assert node.call("WorkItem.RevisionGet", former_query, token) == original_view
        assert node.call("Artifact.Read", {"reference": old_artifact}, token) == old_page
        assert node.call("Artifact.Read", {"reference": new_artifact}, token) == result
        history = node.call("WorkItem.History", current_query, token)["items"]
        assert [entry["reference"] for entry in history] == [original_ref, elevated["reference"]], history
        assert "priority" not in history[0]["value"] and history[1]["value"]["priority"] == "High", history

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 89 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["sqlite_integrity"] == "Verified", report
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format-88 default priority, history, and old Task result survive format-89 migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
