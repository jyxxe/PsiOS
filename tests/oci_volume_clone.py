"""Exercise detached volume-clone custody against a real OCI workload."""

import json
import subprocess
from time import monotonic, sleep

from support import target


TOOL = "volume.clone"


def verify_volume_clone(node, docker, spec, app_ids):
    volumes = node.state / "volumes"
    backups = node.state / "backups"

    def configured(volume, name, label):
        return dict(spec, volume=target(volume), name=name, environment={"TEST_NAME": label})

    def install(volume, name, label):
        app = node.perform("application.install", configured(volume, name, label))
        app_ids.append(app["resource_id"])
        return app

    def submit(tool, params, delegation="owner-delegation"):
        prepared = node.prepare(tool, params, delegation=delegation)
        return node.call("Task.Submit", prepared)["task_id"]

    def fails(tool, params, code, delegation="owner-delegation"):
        task = node.wait(submit(tool, params, delegation), autoapprove=True)
        assert (task["lifecycle"], task["outcome_knowledge"], task["conditions"][0]["type"]) == (
            "Failed", "NotStarted", code,
        ), task
        assert task["result"]["artifact_refs"] == [], task

    def mounted(app, volume):
        health = node.perform("application.health", target(app))
        assert health["process_running"] is True, health
        container = json.loads(subprocess.check_output(docker + ["inspect", health["container_id"]], timeout=10))[0]
        mount, = [entry for entry in container["Mounts"] if entry["Destination"] == "/data"]
        assert mount["Source"] == str(volumes / volume["resource_id"] / "data") and mount["RW"], mount

    def wait_for_starts(path, expected):
        deadline = monotonic() + 10
        while monotonic() < deadline:
            if path.is_file() and path.read_bytes() == expected:
                return
            sleep(0.05)
        raise AssertionError(f"workload did not write expected data to {path}")

    source = node.perform("volume.create", {"name": "clone-oci-source"})
    source_id = source["resource_id"]
    data = volumes / source_id / "data"
    original_app = install(source, "clone-oci-original", "original-instance")
    original_app = node.perform("application.start", target(original_app))
    mounted(original_app, source)
    wait_for_starts(data / "starts", b"original-instance\n")
    source = node.get(source_id)["reference"]
    admission = node.call("Node.WorkAdmission.Get", {})["reference"]
    blockers = node.call("Node.WorkAdmission.Blockers", {"expected_resource_version": admission["version"], "limit": 64})
    original_hold, = [item for item in blockers["exclusive_holds"]["items"] if item["resource_ref"] == source]
    assert original_hold["holder_kind"] == "Application" and original_hold["holder_ref"] == original_app, original_hold

    issued = node.call("Grant.Issue", {
        "principal_id": "local-owner", "actions": ["task.submit", "task.read", TOOL],
        "units": 8, "ttl_seconds": 300, "tool_targets": [source_id, "local"],
    })
    delegation = issued["delegation_ref"]["resource_id"]
    original_volumes = set(volumes.iterdir())
    original_backups = set(backups.iterdir())
    fails("backup.create", target(source), "Conflict")
    fails(TOOL, {"target": target(source), "name": "busy-copy"}, "Conflict", delegation)
    assert set(volumes.iterdir()) == original_volumes and set(backups.iterdir()) == original_backups

    original_app = node.perform("application.stop", target(original_app))
    source = node.get(source_id)["reference"]
    assert (data / "starts").read_bytes() == b"original-instance\n"
    pending = submit(TOOL, {"target": target(source), "name": "clone-oci-target"}, delegation)
    assert node.wait(pending)["lifecycle"] == "WaitingApproval"
    node.approve(pending)
    task = node.wait(pending)
    assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
    artifact, = task["result"]["artifact_refs"]
    clone = node.call("Artifact.Read", {"reference": artifact})
    clone_id = clone["resource_id"]
    clone_data = volumes / clone_id / "data"
    clone_spec = node.get(clone_id)["resource"]["spec"]
    assert clone_spec["cloned_from"] == source and clone_spec["storage_restore_phase"] == "AwaitingActivation", clone_spec
    assert (clone_data / "starts").read_bytes() == b"original-instance\n"
    assert set(volumes.iterdir()) == original_volumes | {clone_data.parent}
    assert set(backups.iterdir()) == original_backups

    applications = node.call("Resource.List", {"kind": "Application"})["items"]
    fails("application.install", configured(clone, "clone-oci-premature", "too-early"), "PreconditionFailed")
    fails("volume.write", {**target(clone), "name": "premature-write", "content": "refused"}, "PreconditionFailed")
    assert node.call("Resource.List", {"kind": "Application"})["items"] == applications
    assert not (clone_data / "premature-write").exists()

    activated = node.perform("volume.activate", target(clone))
    activation = node.get(clone_id)["resource"]["spec"]
    assert activation["storage_restore_phase"] == "Activated" and activation["cloned_from"] == source
    proof = node.call("Resource.Resolve", {"reference": activation["restore_verification_ref"]})["value"]
    assert proof["volume_ref"] == clone and proof["cloned_from"] == source, proof
    cloned_app = install(activated, "clone-oci-successor", "cloned-instance")
    cloned_app = node.perform("application.start", target(cloned_app))
    mounted(cloned_app, activated)
    wait_for_starts(clone_data / "starts", b"original-instance\ncloned-instance\n")
    cloned_app = node.perform("application.stop", target(cloned_app))
    clone = node.get(clone_id)["reference"]
    assert node.perform("volume.read", {**target(clone), "name": "starts"})["content"] == "original-instance\ncloned-instance\n"
    assert node.perform("volume.read", {**target(source), "name": "starts"})["content"] == "original-instance\n"
    assert (data / "starts").stat().st_ino != (clone_data / "starts").stat().st_ino
    node.perform("application.uninstall", target(cloned_app))
    node.perform("application.uninstall", target(original_app))
    assert (data / "starts").read_bytes() == b"original-instance\n"
    assert (clone_data / "starts").read_bytes() == b"original-instance\ncloned-instance\n"
    print("PASS real OCI attachment blocks clone, detached verification gates bind, and workloads write independently")
