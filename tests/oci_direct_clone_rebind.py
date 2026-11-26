"""Rebind a stopped OCI deployment to a verified clone recovered from a lost receipt."""

from contextlib import closing, contextmanager
import json
import os
import signal
import sqlite3
import subprocess
from time import monotonic, sleep

from oci_capture import denied
from support import Failure, target


CLONE = "volume.clone"
INSPECT = "volume.inspect_publication"
REBIND = "application.rebind_restored_volume"


def grant(node, tool, scopes):
    return node.call("Grant.Issue", {"principal_id": "local-owner", "tool_targets": scopes,
        "actions": ["task.submit", "task.read", tool], "units": 8, "ttl_seconds": 300,
    })["delegation_ref"]["resource_id"]


def result(node, task_id):
    task = node.wait(task_id)
    assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
    artifact, = task["result"]["artifact_refs"]
    return node.call("Artifact.Read", {"reference": artifact})


def refuse_task(node, request, code):
    try:
        accepted = node.call("Task.Submit", request)
    except Failure as failure:
        assert failure.code == code, failure
    else:
        task = node.wait(accepted["task_id"], autoapprove=True)
        assert (task["lifecycle"], task["outcome_knowledge"], task["conditions"][0]["type"]) == (
            "Failed", "NotStarted", code,
        ), task
        assert not task["result"]["artifact_refs"], task


@contextmanager
def abort_catalog_insert(node, predicate):
    database = node.state / "state.db"
    with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
        conn.execute(f"""CREATE TRIGGER direct_clone_fixture_catalog_fault BEFORE INSERT ON versions
            WHEN {predicate}
            BEGIN SELECT RAISE(ABORT, 'injected direct clone catalogue commit failure'); END""")
    try:
        yield
    finally:
        with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
            conn.execute("DROP TRIGGER direct_clone_fixture_catalog_fault")


def verify_stopped(node):
    report = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert report["sqlite_integrity"] == "Verified" and not report["migration_required"], report


def recover_direct_clone(node, source, volumes, original_bytes):
    request = node.prepare(CLONE, {"target": target(source), "name": "direct-rebind-candidate"},
                           delegation=grant(node, CLONE, [source["resource_id"], "local"]))
    before = set(volumes.iterdir())
    # Fault only the new Volume catalogue insert, after the filesystem publication has committed.
    with abort_catalog_insert(node, "NEW.kind='Volume' AND NEW.spec->>'name'='direct-rebind-candidate'"):
        submitted = node.call("Task.Submit", request)
        task_id = submitted["task_id"]
        assert node.wait(task_id)["lifecycle"] == "WaitingApproval"
        node.approve(task_id)
        interrupted = node.wait(task_id)
        assert (interrupted["lifecycle"], interrupted["outcome_knowledge"]) == ("Paused", "Unknown"), interrupted
        operation, = interrupted["operation_refs"]
        candidate_id = f"volume-{operation['resource_id']}"
        path = volumes / candidate_id / "data" / "starts"
        assert path.read_bytes() == original_bytes
        identity = (path.stat().st_dev, path.stat().st_ino)
        assert set(volumes.iterdir()) == before | {path.parent.parent}
        denied(node, "NotFound", "Resource.Get", {"id": candidate_id})
        node.stop(signal.SIGKILL)
    verify_stopped(node)
    node.start()
    assert node.wait(task_id) == interrupted
    inspect = node.prepare(INSPECT, target(operation), delegation=grant(node, INSPECT, [operation["resource_id"]]))
    observed_id = node.call("Task.Submit", inspect)["task_id"]
    observed_task = node.wait(observed_id)
    assert (observed_task["lifecycle"], observed_task["outcome_knowledge"]) == ("Succeeded", "Known"), observed_task
    observation, = observed_task["result"]["artifact_refs"]
    evidence = node.call("Artifact.Read", {"reference": observation})
    assert (evidence["operation_ref"], evidence["publication"]) == (operation, "Published"), evidence
    recovered = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": observation})
    assert recovered["outcome_knowledge"] == "Known", recovered
    clone = result(node, task_id)
    assert clone["resource_id"] == candidate_id
    assert (path.stat().st_dev, path.stat().st_ino) == identity
    assert set(volumes.iterdir()) == before | {path.parent.parent}
    assert node.call("Task.Submit", request) == {**submitted, "deduplicated": True}
    return clone, evidence["checkpoint_ref"]


def expect_bytes(path, expected):
    deadline = monotonic() + 10
    while monotonic() < deadline:
        if path.is_file() and path.read_bytes() == expected:
            return
        sleep(0.04)
    raise AssertionError(f"OCI fixture did not write expected bytes to {path}")


def verify_direct_clone_rebind(node, docker, spec, app_ids):
    volumes = node.state / "volumes"
    source = node.perform("volume.create", {"name": "direct-rebind-source"})
    source_id = source["resource_id"]
    source_file = volumes / source_id / "data" / "starts"
    app = node.perform("application.install", dict(spec, name="direct-rebind-app", volume=target(source),
                                                   environment={"TEST_NAME": "direct-clone-runtime"}))
    app_id = app["resource_id"]
    app_ids.append(app_id)
    app = node.perform("application.start", target(app))
    original_bytes = b"direct-clone-runtime\n"
    expect_bytes(source_file, original_bytes)
    app = node.perform("application.stop", target(app))
    source_snapshot = node.get(source_id)["reference"]
    original = node.get(app_id)
    deployment_ref = original["resource"]["spec"]["deployment_ref"]
    release_ref = original["resource"]["spec"]["release_ref"]
    deployment = node.get(deployment_ref["resource_id"])

    def containers():
        return subprocess.check_output([*docker, "ps", "-aq", "--filter", f"label=io.psios.application={app_id}"],
                                       text=True, timeout=10).split()

    assert not containers()
    backups = set((node.state / "backups").iterdir())
    clone, publication = recover_direct_clone(node, source_snapshot, volumes, original_bytes)
    clone_id = clone["resource_id"]
    clone_root = volumes / clone_id / "data"
    clone_file = clone_root / "starts"
    created = node.get(clone_id)["resource"]["spec"]
    assert created["storage_restore_phase"] == "AwaitingActivation", created
    assert created["cloned_from"] == source_snapshot and "restored_from" not in created, created
    assert created["restore_proof"]["includes_mtime"] is True, created
    assert created["restore_proof"]["root_mode_unix_v1"] == clone_root.stat().st_mode & 0o777, created
    assert clone_file.stat().st_ino != source_file.stat().st_ino
    assert set((node.state / "backups").iterdir()) == backups

    source = node.perform("volume.write", {**target(source_snapshot), "name": "starts",
                                           "content": "newer independent source\n"})["volume_ref"]
    assert source != source_snapshot
    assert clone_file.read_bytes() == original_bytes and source_file.read_bytes() == b"newer independent source\n"
    scope = grant(node, REBIND, [app_id, source_id, clone_id])

    def params(candidate):
        return {"target": target(app), "source_volume_ref": source, "restored_volume_ref": candidate}

    def prepare(candidate, delegation=scope):
        return node.prepare(REBIND, params(candidate), delegation=delegation)

    def assert_unbound():
        assert node.get(app_id) == original
        assert node.get(deployment_ref["resource_id"]) == deployment
        assert node.get(clone_id)["reference"] == activated
        assert node.get(source_id)["reference"] == source
        assert source_file.read_bytes() == b"newer independent source\n" and clone_file.read_bytes() == original_bytes

    refuse_task(node, prepare(clone), "PreconditionFailed")
    assert node.get(app_id) == original and not containers()
    activated = node.perform("volume.activate", target(clone))
    activation_spec = node.get(clone_id)["resource"]["spec"]
    activation_ref = activation_spec["restore_verification_ref"]
    activation = node.call("Resource.Resolve", {"reference": activation_ref})["value"]
    assert activation_spec["storage_restore_phase"] == "Activated", activation_spec
    assert (activation["volume_ref"], activation["cloned_from"]) == (clone, source_snapshot), activation
    assert activation["tree_digest"] == created["restore_proof"]["tree_digest"], activation
    assert "backup_ref" not in activation, activation
    refuse_task(node, prepare(clone), "Conflict")
    denied(node, "PermissionDenied", "Task.Submit", prepare(activated, grant(node, REBIND, [app_id, source_id])))

    attribute = "user.psios-direct-rebind-proof"
    stamp = clone_root.stat().st_mtime_ns
    os.setxattr(clone_root, attribute, b"root metadata added after activation")
    try:
        assert clone_root.stat().st_mtime_ns == stamp
        pending = node.call("Task.Submit", prepare(activated))["task_id"]
        assert node.wait(pending)["lifecycle"] == "WaitingApproval"
        node.approve(pending)
        refusal = node.wait(pending)
        assert (refusal["lifecycle"], refusal["outcome_knowledge"], refusal["conditions"][0]["type"]) == (
            "Failed", "NotStarted", "PreconditionFailed",
        ), refusal
        assert "clone root" in refusal["conditions"][0]["message"], refusal
        assert not refusal["result"]["artifact_refs"], refusal
        assert os.getxattr(clone_root, attribute) == b"root metadata added after activation"
        assert_unbound()
        assert not containers()
    finally:
        os.removexattr(clone_root, attribute)

    stray = subprocess.check_output([*docker, "create", "--network", "none", "--label",
        f"io.psios.application={app_id}", spec["image_id"], "/probe"], text=True, timeout=10).strip()
    try:
        refuse_task(node, prepare(activated), "PreconditionFailed")
        assert len(containers()) == 1 and stray.startswith(containers()[0])
        assert_unbound()
    finally:
        subprocess.run([*docker, "rm", stray], check=True, stdout=subprocess.DEVNULL, timeout=10)

    # This fault runs after the original Deployment supersession and attempted successor insert.
    predicate = f"NEW.id='{app_id}' AND NEW.spec->>'volume_id'='{clone_id}'"
    with abort_catalog_insert(node, predicate):
        failed_request = prepare(activated)
        failed = node.call("Task.Submit", failed_request)
        assert node.wait(failed["task_id"])["lifecycle"] == "WaitingApproval"
        node.approve(failed["task_id"])
        failure = node.wait(failed["task_id"])
        assert (failure["lifecycle"], failure["outcome_knowledge"], failure["conditions"][0]["type"]) == (
            "Failed", "NotStarted", "StorageUnavailable",
        ), failure
        assert not failure["result"]["artifact_refs"], failure
        assert_unbound()
        assert not containers()
        with closing(sqlite3.connect(node.state / "state.db")) as conn:
            assert conn.execute("SELECT count(*) FROM versions WHERE kind='Deployment' AND spec->>'volume_id'=?",
                                (clone_id,)).fetchone() == (0,)
        node.stop(signal.SIGKILL)
    verify_stopped(node)
    node.start()
    assert_unbound()
    assert not containers()
    assert node.call("Task.Submit", failed_request) == {**failed, "deduplicated": True}
    assert node.wait(failed["task_id"]) == failure

    request = prepare(activated)
    accepted = node.call("Task.Submit", request)
    pending = node.wait(accepted["task_id"])
    assert pending["lifecycle"] == "WaitingApproval", pending
    approval = node.get(pending["conditions"][0]["message"])["value"]
    plan = node.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
    assert [item["resource_ref"] for item in plan["preconditions"]] == [app, source, activated], plan
    node.approve(accepted["task_id"])
    receipt = result(node, accepted["task_id"])
    assert receipt == {"application_ref": node.get(app_id)["reference"], "deployment_ref": receipt["deployment_ref"],
                       "previous_deployment_ref": deployment_ref, "source_volume_ref": source,
                       "restored_volume_ref": activated}, receipt
    rebound = node.get(app_id)
    assert rebound["value"]["lifecycle"] == "Stopped" and not containers()
    rebound_spec = rebound["resource"]["spec"]
    assert rebound_spec["release_ref"] == release_ref and rebound_spec["volume_id"] == clone_id, rebound_spec
    assert rebound_spec["deployment_ref"] == receipt["deployment_ref"], rebound_spec
    bound = node.call("Resource.Resolve", {"reference": receipt["deployment_ref"]})["resource"]["spec"]["restore_rebind"]
    assert bound["clone_source"] == {"volume_ref": source_snapshot, "publication_ref": publication}, bound
    assert bound["activation_ref"] == activation_ref and "backup_ref" not in bound, bound
    assert node.call("Task.Submit", request) == {**accepted, "deduplicated": True}

    node.stop(signal.SIGKILL)
    verify_stopped(node)
    node.start()
    assert node.get(app_id) == rebound and not containers()
    app = node.perform("application.start", target(receipt["application_ref"]))
    health = node.perform("application.health", target(app))
    assert health["process_running"] is True, health
    running = json.loads(subprocess.check_output([*docker, "inspect", health["container_id"]], timeout=10))[0]
    mount, = [item for item in running["Mounts"] if item["Destination"] == "/data"]
    assert mount["Source"] == str(clone_root) and mount["RW"], mount
    expect_bytes(clone_file, original_bytes * 2)
    app = node.perform("application.stop", target(app))
    assert not containers()
    assert node.perform("volume.read", {**target(source), "name": "starts"})["content"] == "newer independent source\n"
    assert node.perform("volume.read", {**target(node.get(clone_id)["reference"]),
                                        "name": "starts"})["content"] == (original_bytes * 2).decode()
    assert set((node.state / "backups").iterdir()) == backups
    node.perform("application.uninstall", target(app))
    print("PASS recovered direct clone rebind, exact root/engine guards, atomic catalog retry and independent OCI data")
