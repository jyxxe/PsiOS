"""Use an actual format-79 Task result as pinned evidence after the format-80 upgrade."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node, key, target


def verify(node):
    return json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))


def upgrade(previous, current):
    node = Node(previous)
    old_daemon = node.daemon.copy()

    def grant(principal, actions, targets):
        return node.call("Grant.Issue", {
            "principal_id": principal, "actions": actions, "tool_targets": targets,
            "units": 4, "ttl_seconds": 300,
        })

    def rejects(code, method, params, token=None):
        try:
            node.call(method, params, token)
        except Failure as error:
            assert error.code == code, error
        else:
            raise AssertionError(f"{method} unexpectedly succeeded")

    try:
        project_request = {"key": key(), "title": "Accept an existing host observation"}
        rejects("UnsupportedCapability", "Project.Create", project_request)
        contributor = node.call("Agent.Register", {"name": "previous-format-contributor"})
        contributor_id = contributor["principal_id"]
        token = contributor["token"]
        profile = node.get("profile-local")["reference"]
        execution = grant(contributor_id, ["task.submit", "task.read", "system.status"], [profile["resource_id"]])
        request = node.prepare("system.status", {}, token=token, delegation=execution["delegation_ref"]["resource_id"])
        task_id = node.call("Task.Submit", request, token)["task_id"]
        original = node.wait(task_id)
        assert (original["lifecycle"], original["outcome_knowledge"]) == ("Succeeded", "Known"), original
        task = node.get(task_id)["reference"]
        output, = original["result"]["artifact_refs"]
        output_params = {"reference": output}
        payload = node.call("Artifact.Read", output_params, token)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 79 and not old["migration_required"], old
        assert old["node_id"] == payload["authority_id"], (old, payload)
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 79 and before["migration_required"], before
        assert before["node_id"] == old["node_id"]
        node.start()
        assert node.call("Principal.WhoAmI", {}, token)["principal_id"] == contributor_id
        assert node.call("Task.Get", {"id": task_id}, token) == original
        assert node.get(task_id)["reference"] == task
        assert node.call("Artifact.Read", output_params, token) == payload

        reviewer = node.call("Agent.Register", {"name": "post-upgrade-reviewer"})
        project = node.call("Project.Create", project_request)["reference"]
        project_id = project["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        for actor, role, action in ((contributor, "Contributor", "project.contribute"),
                                    (reviewer, "Reviewer", "project.review")):
            principal = actor["principal_id"]
            project = node.call("Project.SetMember", {
                **target(project), "principal_id": principal, "role": role,
            })["reference"]
            grant(principal, ["project.read", action], [project_id])
        item = node.call("WorkItem.Create", {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Use the pre-upgrade status output", "assignee_principal_id": contributor_id,
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        })["reference"]
        pending = node.call("WorkItem.SubmitEvidence", {
            **target(item), "task_ref": task, "artifact_ref": output,
        }, token)
        assert pending["value"]["lifecycle"] == "InReview", pending
        evict = {**target(task), "reason": "attempt to evict linked pre-upgrade output"}
        archived = node.call("Task.Archive", target(task), token)
        assert node.call("Resource.Resolve", {"reference": archived})["value"]["task_ref"] == task
        rejects("Conflict", "Task.EvictResults", evict)

        node.stop(signal.SIGKILL)
        node.start()
        item_query = {"id": item["resource_id"]}
        assert node.call("WorkItem.Get", item_query, token) == pending
        rejects("Conflict", "Task.EvictResults", evict)
        assert node.get(task_id)["reference"] == task
        assert node.get(archived["resource_id"])["reference"] == archived
        reviewer_token = reviewer["token"]
        rejects("PermissionDenied", "Artifact.Read", output_params, reviewer_token)
        grant(reviewer["principal_id"], ["task.results.read"], [task_id])
        assert node.call("Artifact.Read", output_params, reviewer_token) == payload
        accepted = node.call("WorkItem.Review", {**target(pending["reference"]), "accept": True}, reviewer_token)
        evidence = accepted["value"]["evidence"]
        assert accepted["value"]["lifecycle"] == "Verified", accepted
        assert (evidence["task_ref"], evidence["artifact_ref"], evidence["submitted_by"]) == (
            task, output, contributor_id,
        ), evidence
        rejects("Conflict", "Task.EvictResults", evict)
        closed = node.call("WorkItem.Close", target(accepted["reference"]))
        assert closed["value"]["lifecycle"] == "Closed" and closed["value"]["evidence"] == evidence, closed
        node.call("Task.EvictResults", evict)
        rejects("ResultEvicted", "Artifact.Read", output_params, token)

        node.stop()
        after = verify(node)
        assert after["database_format"] >= 80 and not after["migration_required"], after
        assert after["node_id"] == old["node_id"] and after["sqlite_integrity"] == "Verified", after
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 79 task bytes and identity survive migration, crash, and format 80 independent review")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
