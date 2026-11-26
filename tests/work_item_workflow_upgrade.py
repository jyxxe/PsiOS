"""Review an actual format-83 Workflow result under the format-84 WorkItem criterion."""
import argparse
from pathlib import Path
import signal
import subprocess
import time

from project_upgrade import verify
from support import Failure, Node, key, target
from test_schedule import timestamp


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

    try:
        contributor = node.call("Agent.Register", {"name": "format83-workflow-contributor"})
        reviewer = node.call("Agent.Register", {"name": "format83-workflow-reviewer"})
        actor, reviewer_token = contributor["token"], reviewer["token"]
        project = node.call("Project.Create", {"key": key(), "title": "Retain the previous observation"})["reference"]
        project_id = project["resource_id"]
        grant("local-owner", ["project.read", "project.manage"], [project_id])
        for member, role, action in ((contributor, "Contributor", "project.contribute"),
                                     (reviewer, "Reviewer", "project.review")):
            project = node.call("Project.SetMember", {
                **target(project), "principal_id": member["principal_id"], "role": role,
            })["reference"]
            grant(member["principal_id"], ["project.read", action], [project_id])

        profile = node.get("profile-local")["reference"]
        execution = grant(contributor["principal_id"],
                          ["workflow.manage", "resource.read", "task.submit", "task.read", "system.status"],
                          [profile["resource_id"]])
        definition = node.call("Workflow.Define", {
            "key": key(), "steps": [{"id": "observe", "tool": "system.status", "input": {}, "after": []}],
        }, actor)
        run = node.call("Workflow.Start", {
            "key": key(), "definition_ref": definition["reference"], "delegation_id": execution,
            "deadline_at": timestamp(90),
        }, actor)
        run_query = {"id": run["reference"]["resource_id"]}
        limit = time.monotonic() + 30
        while run["state"]["lifecycle"] != "Succeeded":
            assert run["state"]["lifecycle"] not in ("Failed", "Cancelled", "Blocked"), run
            assert time.monotonic() < limit, run
            time.sleep(0.04)
            run = node.call("Workflow.Get", run_query, actor)
        assert run["state"]["outcome_knowledge"] == "Known", run
        step, = run["state"]["steps"]
        assert (step["step_id"], step["lifecycle"], step["outcome_knowledge"]) == ("observe", "Succeeded", "Known")
        task = step["task_ref"]
        result = node.call("Task.Get", {"id": task["resource_id"]}, actor)
        output, = result["result"]["artifact_refs"]
        read = {"reference": output}
        payload = node.call("Artifact.Read", read, actor)

        subject = {
            "project_id": project_id, "expected_project_version": project["version"],
            "assignee_principal_id": contributor["principal_id"],
        }
        legacy = node.call("WorkItem.Create", {
            **subject, "key": key(), "title": "Review the Task result",
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        })
        exact_inputs = {"task_ref": task, "artifact_ref": output}
        legacy = node.call("WorkItem.SubmitEvidence", {**target(legacy["reference"]), **exact_inputs}, actor)
        legacy_query = {"id": legacy["reference"]["resource_id"]}
        assert legacy["value"]["lifecycle"] == "InReview", legacy
        assert "workflow_witness" not in legacy["value"]["evidence"], legacy
        fixed_request = {
            **subject, "key": key(), "title": "Review the exact Workflow step",
            "criterion": {"kind": "WorkflowTaskResult", "definition_ref": definition["reference"],
                          "step_id": "observe", "tool": "system.status", "target_ref": profile},
        }
        rejects("InvalidArgument", "WorkItem.Create", fixed_request)

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 83 and not old["migration_required"], old
        witness = node.witness / "head"
        prior_head = witness.read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        before = verify(node)
        assert before["database_format"] == 83 and before["migration_required"], before
        assert before["node_id"] == old["node_id"] == payload["authority_id"], before
        assert witness.read_bytes() == prior_head
        node.start()

        assert node.call("Workflow.DefinitionGet", {"id": definition["reference"]["resource_id"]}, actor) == definition
        assert node.call("Workflow.Get", run_query, actor) == run
        assert node.get(task["resource_id"])["reference"] == task
        assert node.call("Artifact.Read", read, actor) == payload
        assert node.call("WorkItem.Get", legacy_query, actor) == legacy

        assigned = node.call("WorkItem.Create", fixed_request)
        pending = node.call("WorkItem.SubmitEvidence", {**target(assigned["reference"]), **exact_inputs}, actor)
        proof = pending["value"]["evidence"]
        assert pending["value"]["lifecycle"] == "InReview", pending
        assert (proof["task_ref"], proof["artifact_ref"]) == (task, output), proof
        assert proof["workflow_witness"] == {
            "workflow_run_ref": run["reference"], "step_id": "observe", "step_key": step["step_key"],
            "task_ref": task,
        }, proof
        revision_query = {"reference": pending["reference"]}
        review = {**target(pending["reference"]), "accept": True}
        rejects("PermissionDenied", "WorkItem.Review", review, reviewer_token)
        grant(reviewer["principal_id"], ["task.results.read"], [task["resource_id"]])
        assert node.call("Artifact.Read", read, reviewer_token) == payload
        shared = node.call("WorkItem.RevisionGet", revision_query, reviewer_token)
        scoped = shared["value"]["evidence"]
        assert (scoped["task_ref"], scoped["artifact_ref"]) == (task, output), scoped
        assert "workflow_witness" not in scoped and "definition_ref" not in scoped["criterion"], scoped

        node.stop(signal.SIGKILL)
        node.start()
        assert node.call("WorkItem.RevisionGet", revision_query, reviewer_token) == shared
        verified = node.call("WorkItem.Review", review, reviewer_token)
        assert verified["value"]["lifecycle"] == "Verified", verified
        closed = node.call("WorkItem.Close", target(verified["reference"]))
        assert closed["value"]["lifecycle"] == "Closed", closed
        eviction = {**target(task), "reason": "release the reviewed original workflow output"}
        rejects("Conflict", "Task.EvictResults", eviction)
        accepted_legacy = node.call("WorkItem.Review", {**target(legacy["reference"]), "accept": True}, reviewer_token)
        node.call("WorkItem.Close", target(accepted_legacy["reference"]))
        node.call("Task.EvictResults", eviction)
        rejects("ResultEvicted", "Artifact.Read", read, reviewer_token)
        assert node.call("WorkItem.RevisionGet", revision_query, reviewer_token) == shared

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 84 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["sqlite_integrity"] == "Verified", report
        obsolete = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode != 0 and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format 83 Workflow output supports independently reviewed format 84 evidence alongside its original pin")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
