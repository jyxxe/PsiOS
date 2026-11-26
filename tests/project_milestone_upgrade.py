"""Close a format-83 Milestone using a WorkItem accepted by a real format-82 authority."""
import argparse
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from support import Failure, Node, key, target


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

    def run(tool, inputs, delegation, token):
        prepared = node.prepare(tool, inputs, token=token, delegation=delegation)
        task_id = node.call("Task.Submit", prepared, token)["task_id"]
        result = node.wait(task_id)
        assert (result["lifecycle"], result["outcome_knowledge"]) == ("Succeeded", "Known"), result
        artifact, = result["result"]["artifact_refs"]
        return node.get(task_id)["reference"], artifact

    try:
        project = node.call("Project.Create", {"key": key(), "title": "Preserve already accepted work"})["reference"]
        project_id = project["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        assignee = node.call("Agent.Register", {"name": "format82-milestone-contributor"})
        reviewer = node.call("Agent.Register", {"name": "format82-milestone-reviewer"})
        for agent, role, action in ((assignee, "Contributor", "project.contribute"),
                                    (reviewer, "Reviewer", "project.review")):
            project = node.call("Project.SetMember", {
                **target(project), "principal_id": agent["principal_id"], "role": role,
            })["reference"]
            grant(agent["principal_id"], ["project.read", action], [project_id])
        token, reviewer_token = assignee["token"], reviewer["token"]
        project_view = node.call("Project.Get", {"id": project_id}, reviewer_token)
        profile = node.get("profile-local")["reference"]
        item = node.call("WorkItem.Create", {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Record the host status", "assignee_principal_id": assignee["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        })["reference"]
        item_id = item["resource_id"]
        status_grant = grant(assignee["principal_id"], ["task.submit", "task.read", "system.status"],
                             [profile["resource_id"]])
        task, output = run("system.status", {}, status_grant, token)
        payload = node.call("Artifact.Read", {"reference": output}, token)
        tool = "project.submit_evidence"
        evidence_grant = grant(assignee["principal_id"], ["task.submit", "task.read", tool], [item_id])
        _, submission = run(tool, {**target(item), "task_ref": task, "artifact_ref": output}, evidence_grant, token)
        submitted = node.call("Artifact.Read", {"reference": submission}, token)
        assert submitted["lifecycle"] == "InReview", submitted
        pending = submitted["work_item_ref"]

        review = {**target(pending), "accept": True}
        rejects("PermissionDenied", "WorkItem.Review", review, reviewer_token)
        grant(reviewer["principal_id"], ["task.results.read"], [task["resource_id"]])
        reviewed = node.call("WorkItem.Review", review, reviewer_token)
        assert reviewed["value"]["lifecycle"] == "Verified", reviewed
        assert reviewed["value"]["review"]["reviewer_principal_id"] == reviewer["principal_id"], reviewed
        closed = node.call("WorkItem.Close", target(reviewed["reference"]))
        assert closed["value"]["lifecycle"] == "Closed", closed
        evidence = closed["value"]["evidence"]
        assert (evidence["task_ref"], evidence["artifact_ref"]) == (task, output), evidence
        milestone_request = {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Approve the recorded host inspection", "work_item_ids": [item_id],
        }
        rejects("UnsupportedCapability", "Milestone.Create", milestone_request)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 82 and not old["migration_required"], old
        witness = node.witness / "head"
        prior_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        preview = verify(node)
        assert preview["database_format"] == 82 and preview["migration_required"], preview
        assert preview["node_id"] == old["node_id"] == payload["authority_id"], preview
        assert witness.read_bytes() == prior_head
        node.start()

        assert node.call("Project.Get", {"id": project_id}, reviewer_token) == project_view
        assert node.call("WorkItem.Get", {"id": item_id}) == closed
        assert node.get(task["resource_id"])["reference"] == task
        assert node.call("Artifact.Read", {"reference": output}, reviewer_token) == payload
        history = node.call("WorkItem.History", {"id": item_id}, reviewer_token)
        assert [row["reference"] for row in history["items"]] == [item, pending, reviewed["reference"],
                                                                 closed["reference"]], history
        assert history["next_cursor"] is None, history
        assert node.call("Milestone.List", {"project_id": project_id}, reviewer_token)["items"] == []

        opened = node.call("Milestone.Create", milestone_request)
        assert opened["value"]["lifecycle"] == "Open" and not opened["value"]["unresolved_work_items"], opened
        rejects("PermissionDenied", "Milestone.Close", target(opened["reference"]), reviewer_token)
        milestone = node.call("Milestone.Close", target(opened["reference"]))
        assert milestone["value"]["lifecycle"] == "Closed", milestone
        assert milestone["value"]["acceptance"]["closed_work_item_refs"] == [closed["reference"]], milestone
        node.stop(signal.SIGKILL)
        node.start()

        assert node.call("Milestone.Get", {"id": milestone["reference"]["resource_id"]}, reviewer_token) == milestone
        assert node.call("Milestone.Create", milestone_request) == milestone
        assert node.call("WorkItem.Get", {"id": item_id}) == closed
        assert node.call("Artifact.Read", {"reference": submission}, token) == submitted
        assert node.call("Artifact.Read", {"reference": output}, reviewer_token) == payload

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 83 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["sqlite_integrity"] == "Verified", report
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 82 closed WorkItem and its original Task output support durable format 83 milestone acceptance")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
