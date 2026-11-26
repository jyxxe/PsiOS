"""Recover a stopped OCI application onto an independently restored local volume."""

from contextlib import closing
import json
import signal
import sqlite3
import subprocess

from oci_capture import denied
from support import Failure, target


TOOL = "application.rebind_restored_volume"


def verify_rebind(node, docker, specification, app_ids):
    source = node.perform("volume.create", {"name": "rebind-source"})
    source_id = source["resource_id"]
    app = node.perform("application.install", dict(specification, name="rebind-app", volume=target(source),
                                                  environment={"TEST_NAME": "recovered"}))
    app_id = app["resource_id"]
    app_ids.append(app_id)
    app = node.perform("application.start", target(app))
    app = node.perform("application.stop", target(app))
    source = node.get(source_id)["reference"]
    node.perform("volume.write", dict(**target(source), name="starts", content="verified backup\n"))
    source = node.get(source_id)["reference"]
    original = node.get(app_id)
    original_release = original["resource"]["spec"]["release_ref"]
    original_deployment = original["resource"]["spec"]["deployment_ref"]
    backup = node.perform("backup.create", target(source))
    node.perform("volume.write", dict(**target(source), name="starts", content="newer source data\n"))
    source = node.get(source_id)["reference"]
    restored = node.perform("backup.restore", target(backup))
    restored_id = restored["resource_id"]

    def inputs(*, app_ref=None, source_ref=None, restored_ref=None):
        return {"target": target(app if app_ref is None else app_ref),
                "source_volume_ref": source if source_ref is None else source_ref,
                "restored_volume_ref": restored if restored_ref is None else restored_ref}

    def absent():
        return not subprocess.check_output(docker + ["ps", "-aq", "--filter", "label=io.psios.application=" + app_id],
                                           timeout=10).strip()

    def grant(scopes):
        return node.call("Grant.Issue", {"principal_id": "local-owner", "actions": ["task.submit", "task.read", TOOL],
            "units": 8, "ttl_seconds": 300, "tool_targets": scopes})["delegation_ref"]["resource_id"]

    def prepared(params, delegation=None):
        if delegation is None:
            delegation = grant([app_id, source_id, params["restored_volume_ref"]["resource_id"]])
        return node.prepare(TOOL, params, delegation=delegation)

    def submit(params, delegation=None):
        return node.call("Task.Submit", prepared(params, delegation))

    def reject(params, code="PreconditionFailed", unregistered_container=False):
        try:
            task = submit(params)["task_id"]
        except Failure as error:
            assert error.code == code, error
        else:
            settlement = node.wait(task, autoapprove=True)
            assert settlement["lifecycle"] == "Failed" and settlement["conditions"][0]["type"] == code, settlement
            assert not settlement["result"]["artifact_refs"], settlement
        assert node.get(app_id)["reference"] == app and absent() != unregistered_container

    reject(inputs())
    restored = node.perform("volume.activate", target(restored))
    extra = node.perform("volume.create", {"name": "other-rebind-lineage"})
    extra_backup = node.perform("backup.create", target(extra))
    extra_clone = node.perform("backup.restore", target(extra_backup))
    extra_clone = node.perform("volume.activate", target(extra_clone))
    reject(inputs(restored_ref=extra_clone))

    changed = node.perform("backup.restore", target(backup))
    changed = node.perform("volume.activate", target(changed))
    node.perform("volume.write", dict(**target(changed), name="starts", content="changed successor\n"))
    changed = node.get(changed["resource_id"])["reference"]
    reject(inputs(restored_ref=changed))
    assert node.get(restored_id)["reference"] == restored

    denied(node, "PermissionDenied", "Task.Submit", prepared(inputs(), grant([app_id, source_id])))
    ordinary_scope = grant([app_id, source_id, restored_id])
    pending = submit(inputs(), ordinary_scope)["task_id"]
    waiting = node.wait(pending)
    assert waiting["lifecycle"] == "WaitingApproval", waiting
    approval = node.get(waiting["conditions"][0]["message"])["value"]
    plan = node.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
    assert [condition["resource_ref"] for condition in plan["preconditions"]] == [app, source, restored]
    node.perform("volume.write", dict(**target(source), name="starts", content="newer source data\n"))
    source = node.get(source_id)["reference"]
    node.approve(pending)
    invalid = node.wait(pending)
    assert (invalid["lifecycle"], invalid["outcome_knowledge"], invalid["conditions"][0]["type"]) == (
        "Failed", "NotStarted", "Conflict"), invalid
    assert node.get(app_id)["reference"] == app and absent()

    stray = subprocess.check_output(docker + ["create", "--network", "none", "--label",
        "io.psios.application=" + app_id, specification["image_id"], "/probe"], text=True, timeout=10).strip()
    try:
        reject(inputs(), unregistered_container=True)
    finally:
        subprocess.run(docker + ["rm", stray], check=True, stdout=subprocess.DEVNULL, timeout=10)

    database = node.state / "state.db"
    with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
        conn.execute(f"""CREATE TRIGGER reject_verified_volume_rebind BEFORE INSERT ON versions
            WHEN NEW.id='{app_id}' AND NEW.spec->>'volume_id'='{restored_id}'
            BEGIN SELECT RAISE(ABORT, 'injected application binding publication fault'); END""")
    try:
        failed = node.wait(submit(inputs(), ordinary_scope)["task_id"], autoapprove=True)
        assert (failed["lifecycle"], failed["outcome_knowledge"]) == ("Failed", "NotStarted"), failed
        assert not failed["result"]["artifact_refs"], failed
        assert node.get(app_id) == original and absent()
        with closing(sqlite3.connect(database)) as conn:
            assert conn.execute("SELECT count(*) FROM versions WHERE kind='Deployment' AND spec->>'volume_id'=?",
                                (restored_id,)).fetchone() == (0,)
    finally:
        with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
            conn.execute("DROP TRIGGER reject_verified_volume_rebind")
    node.stop(signal.SIGKILL)
    node.start()

    request = prepared(inputs(), ordinary_scope)
    accepted = node.call("Task.Submit", request)
    pending = node.wait(accepted["task_id"])
    assert pending["lifecycle"] == "WaitingApproval", pending
    assert node.get(app_id) == original and absent()
    node.stop(signal.SIGKILL)
    node.start()
    node.approve(accepted["task_id"])
    settled = node.wait(accepted["task_id"])
    assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
    receipt = node.call("Artifact.Read", {"reference": settled["result"]["artifact_refs"][0]})
    assert receipt["previous_deployment_ref"] == original_deployment
    assert receipt["source_volume_ref"] == source and receipt["restored_volume_ref"] == restored
    app = receipt["application_ref"]
    assert node.call("Task.Submit", request) == {**accepted, "deduplicated": True} and absent()
    rebound = node.get(app_id)
    assert rebound["reference"] == app
    assert rebound["resource"]["spec"]["volume_id"] == restored_id
    assert rebound["resource"]["spec"]["release_ref"] == original_release
    assert rebound["resource"]["spec"]["deployment_ref"] == receipt["deployment_ref"]
    assert rebound["value"]["lifecycle"] == "Stopped", rebound

    node.stop(signal.SIGKILL)
    subprocess.run(node.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)
    node.start()
    app = node.perform("application.start", target(app))
    instance = node.perform("application.health", target(app))["container_id"]
    container = json.loads(subprocess.check_output(docker + ["inspect", instance], timeout=10))[0]
    mounts = [entry for entry in container["Mounts"] if entry["Destination"] == "/data"]
    assert len(mounts) == 1 and mounts[0]["Source"] == str(node.state / "volumes" / restored_id / "data"), mounts
    app = node.perform("application.stop", target(app))
    assert absent()
    restored = node.get(restored_id)["reference"]
    assert node.perform("volume.read", dict(**target(restored), name="starts"))["content"] == "verified backup\nrecovered\n"
    assert node.perform("volume.read", dict(**target(source), name="starts"))["content"] == "newer source data\n"
    node.perform("application.uninstall", target(app))
    print("PASS approved OCI recovery to the verified volume clone, interrupted atomic binding, and retained source")
