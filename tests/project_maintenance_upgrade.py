"""Abandon a real format-80 WorkItem after upgrading its live evidence to format 81."""
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
        })

    def rejects(code, method, params, token=None):
        try:
            node.call(method, params, token)
        except Failure as error:
            assert error.code == code, error
        else:
            raise AssertionError(f"{method} unexpectedly succeeded")

    try:
        contributor = node.call("Agent.Register", {"name": "format80-contributor"})
        reviewer = node.call("Agent.Register", {"name": "format80-reviewer"})
        principal, token = contributor["principal_id"], contributor["token"]
        project = node.call("Project.Create", {"key": key(), "title": "Preserve pending work"})["reference"]
        project_id = project["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        for member, role, action in ((contributor, "Contributor", "project.contribute"),
                                     (reviewer, "Reviewer", "project.review")):
            project = node.call("Project.SetMember", {
                **target(project), "principal_id": member["principal_id"], "role": role,
            })["reference"]
            grant(member["principal_id"], ["project.read", action], [project_id])
        roster = node.call("Project.Get", {"id": project_id}, reviewer["token"])

        profile = node.get("profile-local")["reference"]
        item = node.call("WorkItem.Create", {
            "project_id": project_id, "expected_project_version": project["version"],
            "key": key(), "title": "Record the current system status", "assignee_principal_id": principal,
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        })["reference"]
        execution = grant(principal, ["task.submit", "task.read", "system.status"], [profile["resource_id"]])
        request = node.prepare("system.status", {}, token=token, delegation=execution["delegation_ref"]["resource_id"])
        task_id = node.call("Task.Submit", request, token)["task_id"]
        original = node.wait(task_id)
        assert (original["lifecycle"], original["outcome_knowledge"]) == ("Succeeded", "Known"), original
        task = node.get(task_id)["reference"]
        artifact, = original["result"]["artifact_refs"]
        output = {"reference": artifact}
        payload = node.call("Artifact.Read", output, token)
        pending = node.call("WorkItem.SubmitEvidence", {
            **target(item), "task_ref": task, "artifact_ref": artifact,
        }, token)
        assert pending["value"]["lifecycle"] == "InReview", pending
        assert "abandonment" not in pending["value"], pending
        evidence = pending["value"]["evidence"]
        assert (evidence["task_ref"], evidence["artifact_ref"]) == (task, artifact), evidence
        abandon = {**target(pending["reference"]), "reason": "superseded during authority upgrade"}
        evict = {**target(task), "reason": "release abandoned observation bytes"}
        rejects("UnsupportedCapability", "WorkItem.Abandon", abandon)
        rejects("Conflict", "Task.EvictResults", evict)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 80 and not old["migration_required"], old
        witness = node.witness / "head"
        original_witness = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 80 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == payload["authority_id"], before
        assert witness.read_bytes() == original_witness
        node.start()

        where = {"id": item["resource_id"]}
        assert node.call("Project.Get", {"id": project_id}, reviewer["token"]) == roster
        recovered = node.call("WorkItem.Get", where, token)
        assert recovered["reference"] == pending["reference"], recovered
        assert recovered["value"]["lifecycle"] == "InReview" and recovered["value"]["evidence"] == evidence, recovered
        assert node.get(task_id)["reference"] == task
        assert node.call("Artifact.Read", output, token) == payload
        rejects("Conflict", "Task.EvictResults", evict)

        abandoned = node.call("WorkItem.Abandon", abandon)
        assert abandoned["value"]["lifecycle"] == "Abandoned" and abandoned["value"]["has_evidence"], abandoned
        assert abandoned["value"]["abandonment"]["prior_lifecycle"] == "InReview", abandoned
        assert abandoned["value"].get("evidence") is None, abandoned
        node.stop(signal.SIGKILL)
        node.start()
        final = node.call("WorkItem.Get", where, token)
        assert final["reference"] == abandoned["reference"] and final["value"]["evidence"] == evidence, final
        assert final["value"]["abandonment"] == abandoned["value"]["abandonment"], final
        node.call("Task.EvictResults", evict)
        rejects("ResultEvicted", "Artifact.Read", output, token)

        node.stop()
        after = verify(node)
        assert after["database_format"] >= 81 and not after["migration_required"], after
        assert after["node_id"] == old["node_id"] and after["sqlite_integrity"] == "Verified", after
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 80 active evidence survives upgrade, crash, and format 81 abandonment")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
