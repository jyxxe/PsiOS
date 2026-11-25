"""Actual engine acceptance for scoped OCI restart and lost receipt recovery."""
import json
import os
import signal
import sqlite3
import subprocess

from oci_recovery import check_kernel_limits
from support import Failure, target


def verify_restart(node, docker, spec, app_ids):
    volume = node.perform("volume.create", {"name": "oci-restart-data"})
    app = node.perform("application.install", dict(spec, name="restart-acceptance", volume=target(volume),
                                                   environment={"TEST_NAME": "restart"}))
    app_ids.append(app["resource_id"])
    app = node.perform("application.start", target(app))
    volume = node.get(volume["resource_id"])["reference"]
    starts = node.state / "volumes" / volume["resource_id"] / "data" / "starts"
    actions = ["task.submit", "task.read", "application.inspect_instance", "application.restart"]
    grant_request = {"principal_id": "local-owner", "actions": actions, "ttl_seconds": 300, "units": 16}
    try:
        node.call("Grant.Issue", grant_request)
    except Failure as denied:
        assert denied.code == "InvalidArgument", denied
    else:
        raise AssertionError("unscoped OCI restart grant was accepted")
    limited = node.call("Grant.Issue", {**grant_request, "tool_targets": [app["resource_id"]]})
    request = node.prepare("application.inspect_instance", target(app),
                           delegation=limited["delegation_ref"]["resource_id"])
    try:
        node.call("Task.Submit", request)
    except Failure as denied:
        assert denied.code == "PermissionDenied", denied
    else:
        raise AssertionError("OCI inspection bypassed the bound volume scope")
    grant = node.call("Grant.Issue", {**grant_request, "tool_targets": [app["resource_id"], volume["resource_id"]]})
    delegation = grant["delegation_ref"]["resource_id"]

    def submit(tool, command):
        return node.call("Task.Submit", node.prepare(tool, command, delegation=delegation))["task_id"]

    def inspect(ref):
        state = node.wait(submit("application.inspect_instance", target(ref)))
        assert state["lifecycle"] == "Succeeded", state
        artifact = state["result"]["artifact_refs"][0]
        return node.call("Artifact.Read", {"reference": artifact}), artifact

    def params(ref, observation):
        return {**target(ref), "expected_boot_id": observation["boot_id"], "expected_started_at": observation["started_at"]}

    def real_start(cid):
        return subprocess.check_output(docker + ["inspect", "--format", "{{.State.StartedAt}}", cid], text=True).strip()

    first, old_proof = inspect(app)
    cid = first["container_id"]
    assert real_start(cid) == first["started_at"] and starts.read_text() == "restart\n", first
    bad = dict(params(app, first), expected_started_at="1970-01-01T00:00:00Z")
    refused = node.wait(submit("application.restart", bad), autoapprove=True)
    assert refused["lifecycle"] == "Failed" and refused["conditions"][0]["type"] == "PreconditionFailed", refused
    assert real_start(cid) == first["started_at"] and starts.read_text() == "restart\n"

    accepted = submit("application.restart", params(app, first))
    waiting = node.wait(accepted)
    assert waiting["lifecycle"] == "WaitingApproval", waiting
    assert real_start(cid) == first["started_at"]
    node.approve(accepted)
    completed = node.wait(accepted)
    assert completed["lifecycle"] == "Succeeded", completed
    app = node.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
    second, _ = inspect(app)
    assert second["container_id"] == cid and second["started_at"] != first["started_at"], second
    assert real_start(cid) == second["started_at"] and node.get(volume["resource_id"])["reference"] == volume
    if os.getenv("PSIOS_TEST_OCI_KERNEL_LIMITS"):
        check_kernel_limits(second, docker, cid)

    app_id = app["resource_id"]
    with sqlite3.connect(node.state / "state.db", timeout=10) as database:
        database.execute(f"""CREATE TRIGGER lose_oci_restart BEFORE INSERT ON versions
            WHEN NEW.id='{app_id}' AND NEW.body->'last_restart_checkpoint_ref' IS NOT NULL
            BEGIN SELECT RAISE(ABORT, 'injected OCI restart receipt loss'); END""")
    lost_task = submit("application.restart", params(app, second))
    lost = node.wait(lost_task, autoapprove=True)
    assert lost["lifecycle"] == "Paused" and lost["outcome_knowledge"] == "Unknown", lost
    actual = real_start(cid)
    assert actual != second["started_at"]
    node.stop(signal.SIGKILL)
    with sqlite3.connect(node.state / "state.db", timeout=10) as database:
        database.execute("DROP TRIGGER lose_oci_restart")
    node.start()
    assert node.wait(lost_task) == lost
    competing = node.wait(node.submit("application.stop", target(app)), autoapprove=True)
    assert competing["conditions"][0]["type"] == "Conflict", competing
    operation = lost["operation_refs"][0]
    try:
        node.call("Operation.Reconcile", {**target(operation), "evidence_ref": old_proof})
    except Failure as stale:
        assert stale.code == "OutcomeUnknown", stale
    else:
        raise AssertionError("a pre-restart inspection settled the unknown operation")
    node.stop()
    settings = json.loads(node.config.read_text())
    node.config.write_text(json.dumps(dict(settings, queue_limit=1, recovery_queue_limit=1)))
    node.start()
    used = node.call("Node.Get", {})["admission"]
    assert used["ordinary"] == {"used": 1, "limit": 1}, used
    observed, proof = inspect(app)
    proof_task = node.call("Artifact.Get", {"reference": proof})["resource"]["spec"]["task_id"]
    admission = node.get(f"recovery-admission-{proof_task}")["value"]
    assert admission["unresolved_operation_ref"] == operation, admission
    assert observed["started_at"] == actual and observed["container_id"] == cid, observed
    recovered = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": proof})
    assert recovered["outcome_knowledge"] == "Known", recovered
    settled = node.wait(lost_task)
    assert settled["lifecycle"] == "Succeeded", settled
    app = node.call("Artifact.Read", {"reference": settled["result"]["artifact_refs"][0]})
    assert real_start(cid) == actual and node.get(volume["resource_id"])["reference"] == volume
    app = node.perform("application.stop", target(app))
    node.perform("application.uninstall", target(app))
    assert starts.read_text() == "restart\n" * 3, starts.read_text()
