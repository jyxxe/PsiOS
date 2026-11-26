"""Add and close a child for a WorkItem created by a real format-84 authority."""
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

    def status(execution, token):
        request = node.prepare("system.status", {}, token=token, delegation=execution)
        task_id = node.call("Task.Submit", request, token)["task_id"]
        result = node.wait(task_id)
        assert (result["lifecycle"], result["outcome_knowledge"]) == ("Succeeded", "Known"), result
        artifact, = result["result"]["artifact_refs"]
        return node.get(task_id)["reference"], artifact

    def independently_verify(item, task, artifact):
        pending = node.call("WorkItem.SubmitEvidence", {
            **target(item["reference"]), "task_ref": task, "artifact_ref": artifact,
        }, token)
        assert pending["value"]["lifecycle"] == "InReview", pending
        review = {**target(pending["reference"]), "accept": True}
        rejects("PermissionDenied", "WorkItem.Review", review, reviewer_token)
        grant(reviewer["principal_id"], ["task.results.read"], [task["resource_id"]])
        accepted = node.call("WorkItem.Review", review, reviewer_token)
        evidence = accepted["value"]["evidence"]
        assert accepted["value"]["lifecycle"] == "Verified", accepted
        assert accepted["value"]["review"]["reviewer_principal_id"] == reviewer["principal_id"], accepted
        assert (evidence["task_ref"], evidence["artifact_ref"]) == (task, artifact), evidence
        return accepted

    try:
        assignee = node.call("Agent.Register", {"name": "format84-hierarchy-contributor"})
        reviewer = node.call("Agent.Register", {"name": "format84-hierarchy-reviewer"})
        token, reviewer_token = assignee["token"], reviewer["token"]
        project = node.call("Project.Create", {"key": key(), "title": "Keep the existing delivery objective"})
        project_ref = project["reference"]
        project_id = project_ref["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        for member, role, action in ((assignee, "Contributor", "project.contribute"),
                                     (reviewer, "Reviewer", "project.review")):
            project_ref = node.call("Project.SetMember", {
                **target(project_ref), "principal_id": member["principal_id"], "role": role,
            })["reference"]
            grant(member["principal_id"], ["project.read", action], [project_id])
        roster = node.call("Project.Get", {"id": project_id}, reviewer_token)
        profile = node.get("profile-local")["reference"]
        parent_request = {
            "project_id": project_id, "expected_project_version": project_ref["version"],
            "key": key(), "title": "Accept the host inspection", "assignee_principal_id": assignee["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        }
        parent = node.call("WorkItem.Create", parent_request)
        assert parent["value"]["lifecycle"] == "Ready" and "parent_id" not in parent["value"], parent
        parent_id = parent["reference"]["resource_id"]
        parent_query = {"id": parent_id}
        child_request = {**parent_request, "key": key(), "title": "Check a fresh observation", "parent_id": parent_id}
        execution = grant(assignee["principal_id"], ["task.submit", "task.read", "system.status"],
                          [profile["resource_id"]])
        old_task, old_artifact = status(execution, token)
        old_output = {"reference": old_artifact}
        payload = node.call("Artifact.Read", old_output, token)
        rejects("UnsupportedCapability", "WorkItem.Children", parent_query, reviewer_token)
        rejects("InvalidArgument", "WorkItem.Create", child_request)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 84 and not old["migration_required"], old
        witness = node.witness / "head"
        prior_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 84 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == payload["authority_id"], before
        assert witness.read_bytes() == prior_head
        node.start()

        assert node.call("Project.Get", {"id": project_id}, reviewer_token) == roster
        assert node.call("WorkItem.Get", parent_query) == parent
        assert node.call("WorkItem.Create", parent_request) == parent
        assert node.get(old_task["resource_id"])["reference"] == old_task
        assert node.call("Artifact.Read", old_output, token) == payload
        assert node.call("WorkItem.Children", parent_query, reviewer_token) == {"items": [], "next_after_id": None}
        child = node.call("WorkItem.Create", child_request)
        assert child["value"]["parent_id"] == parent_id and child["value"]["lifecycle"] == "Ready", child
        assert node.call("WorkItem.Create", parent_request) == parent

        parent_verified = independently_verify(parent, old_task, old_artifact)
        close_parent = target(parent_verified["reference"])
        rejects("PreconditionFailed", "WorkItem.Close", close_parent)
        rejects("Conflict", "Task.EvictResults", {**target(old_task), "reason": "parent review still retains output"})
        node.stop(signal.SIGKILL)
        node.start()
        assert node.call("WorkItem.Get", parent_query)["reference"] == parent_verified["reference"]
        rejects("PreconditionFailed", "WorkItem.Close", close_parent)

        new_task, new_artifact = status(execution, token)
        assert new_task != old_task and new_artifact != old_artifact
        verified_child = independently_verify(child, new_task, new_artifact)
        rejects("PreconditionFailed", "WorkItem.Close", close_parent)
        closed_child = node.call("WorkItem.Close", target(verified_child["reference"]))
        assert closed_child["value"]["lifecycle"] == "Closed", closed_child
        closed_parent = node.call("WorkItem.Close", close_parent)
        assert closed_parent["value"]["lifecycle"] == "Closed", closed_parent

        node.stop(signal.SIGKILL)
        node.start()
        assert node.call("WorkItem.Create", parent_request) == closed_parent
        assert node.call("WorkItem.Create", child_request) == closed_child
        recovered, = node.call("WorkItem.Children", parent_query, reviewer_token)["items"]
        assert recovered["reference"] == closed_child["reference"] and recovered["value"]["parent_id"] == parent_id
        assert node.call("WorkItem.RevisionGet", {"reference": parent["reference"]})["value"]["lifecycle"] == "Ready"
        assert node.call("Artifact.Read", old_output, reviewer_token) == payload

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 85 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["sqlite_integrity"] == "Verified", report
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 84 WorkItem and Task become a durable parent hierarchy in format 85")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
