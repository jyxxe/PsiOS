"""Explicit live-engine acceptance. Builds a unique scratch image without a registry or network."""
import argparse
from contextlib import contextmanager
import io
import json
import os
import sqlite3
import subprocess
import tarfile
import threading

from support import Failure, Node, key, target
from oci_recovery import ReceiptBarrier, check_kernel_limits, continue_after_lost_stop, lose_start_receipt, lose_stop_receipt
from oci_capture import TOOL as CAPTURE, denied
from oci_health import verify_image_health
from oci_restart import verify_restart
from test_artifact_install import TOOL as ARTIFACT_INSTALL, grant as artifact_grant
from test_artifact_ingress import chunk, upload_request
from test_oci_publisher import local_signature
from test_session_output import generate_artifact


ARCHIVED_MEDIA = "application/vnd.psios.oci-install.v2+json"


def capture_image(node, image_id):
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    engine = subprocess.check_output(["docker", "--host", "unix:///var/run/docker.sock", "info", "--format", "{{.ID}}"],
                                     text=True, timeout=15).strip()
    allowed = artifact_grant(node, "local-owner", ["task.submit", "task.read", CAPTURE], [node_id])
    request = {"image_id": image_id, "expected_engine_id": engine}
    task = node.call("Task.Submit", node.prepare(CAPTURE, request, delegation=allowed["delegation_ref"]["resource_id"]))["task_id"]
    settled = node.wait(task, autoapprove=True)
    assert settled["lifecycle"] == "Succeeded", settled
    receipt = node.call("Artifact.Read", {"reference": settled["result"]["artifact_refs"][0]})
    return receipt["artifact_ref"], receipt["capture_ref"]


def archived_declaration(specification, archive):
    return json.dumps({"format": "PsiOSLocalOciInstallV2", "install": specification, "image_archive_ref": archive})


def assert_retained(node, source, archive, reason):
    for artifact in (source, archive):
        denied(node, "Conflict", "Artifact.Withdraw", {"reference": artifact, "reason": reason})


@contextmanager
def corrupted_archive(node, archive):
    payload = node.state / "oci-archives" / archive["resource_id"] / "payload"
    if payload.is_file():
        original = payload.read_bytes()

        def write(data):
            payload.chmod(0o600)
            try:
                payload.write_bytes(data)
            finally:
                payload.chmod(0o400)

        write(bytes(len(original)))
        try:
            yield
        finally:
            write(original)
        return
    identity = (archive["resource_id"],)
    with sqlite3.connect(node.state / "state.db") as db:
        original, = db.execute("SELECT payload FROM artifact_chunks WHERE artifact_id=? AND chunk_index=0", identity).fetchone()
        db.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload)) WHERE artifact_id=? AND chunk_index=0", identity)
    try:
        yield
    finally:
        with sqlite3.connect(node.state / "state.db") as db:
            db.execute("UPDATE artifact_chunks SET payload=? WHERE artifact_id=? AND chunk_index=0", (original, *identity))


def fence_after_container_creation(node, docker, app, archive):
    original = json.loads(node.config.read_text())
    socket = node.base / "start-checkpoint.sock"
    node.stop()
    with ReceiptBarrier(socket, original.get("docker_socket", "/var/run/docker.sock"),
                        lambda req: b"POST " in req and b"/containers/create" in req,
                        status=b" 201 ", forward=True) as barrier:
        server = threading.Thread(target=barrier.serve_forever, daemon=True)
        server.start()
        try:
            node.config.write_text(json.dumps(dict(original, docker_socket=str(socket))))
            node.start()
            task = node.submit("application.start", target(app))
            node.approve(task)
            assert barrier.confirmed.wait(12), "Docker did not return a creation response"
            containers = docker + ["ps", "-aq", "--filter", "label=io.psios.application=" + app["resource_id"]]
            assert subprocess.check_output(containers, timeout=10).strip(), "expected a real unstarted container"
            with corrupted_archive(node, archive):
                barrier.release.set()
                failed = node.wait(task)
                assert (failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]) == (
                    "Failed", "Known", "RecoveryRequired"), failed
                assert not subprocess.check_output(containers, timeout=10).strip(), failed
        finally:
            barrier.release.set()
            node.stop()
            barrier.shutdown()
            server.join(3)
            node.config.write_text(json.dumps(original))
    node.start()


def install_from_artifact(node, specification):
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    artifact_grant(node, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
    archive, capture = capture_image(node, specification["image_id"])
    declaration = archived_declaration(specification, archive).encode()
    stage = node.call("Artifact.Stage", upload_request(declaration, declared_media_type=ARCHIVED_MEDIA))
    node.call("Artifact.Stage.PutChunk", chunk(stage, 0, declaration))
    artifact = node.call("Artifact.Publish", {"stage_id": stage["stage_id"]})["artifact_ref"]
    signature, _, _ = local_signature(node, artifact)
    scope = [specification["volume"]["id"], artifact["resource_id"], signature["key_ref"]["resource_id"]]
    actions = ["task.submit", "task.read", ARTIFACT_INSTALL]
    missing = artifact_grant(node, "local-owner", actions, scope)
    allow = artifact_grant(node, "local-owner", actions, [*scope, archive["resource_id"]])
    params = {"reference": artifact, "key": key(), "signature": signature}
    denied(node, "PermissionDenied", "Application.PrepareArtifactInstall",
           {**params, "delegation_id": missing["delegation_ref"]["resource_id"]})
    ready = node.call("Application.PrepareArtifactInstall", {**params, "delegation_id": allow["delegation_ref"]["resource_id"]})
    assert ready["image_archive_ref"] == archive and ready["resource_refs"][-1] == archive
    cancelled = node.call("Task.Submit", ready["request"])["task_id"]
    assert node.wait(cancelled)["lifecycle"] == "WaitingApproval"
    assert_retained(node, artifact, archive, "pending cancelled trial")
    assert node.call("Task.Cancel", target(node.get(cancelled)["reference"]))["lifecycle"] == "Cancelled"
    with sqlite3.connect(node.state / "state.db") as db:
        for table in ("application_artifact_pins", "application_image_archive_pins"):
            assert db.execute(f"SELECT artifact FROM {table} WHERE kind='Task' AND holder=?", (cancelled,)).fetchall() == []
    ready = node.call("Application.PrepareArtifactInstall", {**params, "key": key(),
                      "delegation_id": allow["delegation_ref"]["resource_id"]})
    task_id = node.call("Task.Submit", ready["request"])["task_id"]
    assert node.wait(task_id)["lifecycle"] == "WaitingApproval"
    assert_retained(node, artifact, archive, "pending installation")
    node.stop()
    subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)
    node.start()
    assert_retained(node, artifact, archive, "restarted pending installation")
    completed = node.wait(task_id, autoapprove=True)
    assert completed["lifecycle"] == "Succeeded", completed
    assert_retained(node, artifact, archive, "retained release")
    reference, = completed["result"]["artifact_refs"]
    receipt = node.call("Artifact.Read", {"reference": reference})
    assert receipt["source_artifact_ref"] == artifact, receipt
    assert receipt["image_archive_ref"] == archive and receipt["image_capture_ref"] == capture, receipt
    evidence = node.call("Resource.Resolve", {"reference": receipt["installation_ref"]})["value"]
    assert evidence["deployment_ref"] == receipt["deployment_ref"]
    assert receipt["operation_id"] == completed["operation_refs"][0]["resource_id"]
    assert node.get(receipt["application_ref"]["resource_id"])["value"]["lifecycle"] == "Stopped"
    node.stop()
    subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)
    with sqlite3.connect(node.state / "state.db") as db:
        pin = db.execute("SELECT artifact,holder,kind FROM application_image_archive_pins WHERE holder=?",
                         (receipt["release_ref"]["resource_id"],)).fetchone()
        assert pin == (archive["resource_id"], receipt["release_ref"]["resource_id"], "Release"), pin
        db.execute("DELETE FROM application_image_archive_pins WHERE kind='Release' AND holder=?", (pin[1],))
    lost = subprocess.run(node.daemon + ["verify"], capture_output=True, timeout=15)
    assert lost.returncode != 0 and b"OCI archive disagrees with its retention pin" in lost.stderr, lost
    with sqlite3.connect(node.state / "state.db") as db:
        db.execute("INSERT INTO application_image_archive_pins VALUES(?,?,?)", pin)
    node.start()
    print("Installed a real local OCI image with a signed captured archive", flush=True)
    return receipt["application_ref"], artifact, archive


def stage_from_artifact(node, app, specification):
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    artifact_grant(node, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
    archive, capture = capture_image(node, specification["image_id"])
    declaration = archived_declaration(specification, archive)
    source = generate_artifact(node, "local-owner", None, declaration, ARCHIVED_MEDIA)
    provenance = node.call("Artifact.Get", {"reference": source})["resource"]["spec"]["source"]
    assert provenance["kind"] == "LocalSessionOutput" and provenance["session_ref"]["resource_id"]
    signature, _, publisher = local_signature(node, source)
    signer = signature["key_ref"]
    allow = artifact_grant(node, "local-owner", ["task.submit", "task.read", "application.stage_artifact_release"],
                          [app["resource_id"], source["resource_id"], signer["resource_id"], archive["resource_id"]])
    app_file, source_file, signature_file = (node.base / f"{name}.json" for name in ("app-ref", "source-ref", "signature"))
    for path, value in [(app_file, app), (source_file, source), (signature_file, signature)]:
        path.write_text(json.dumps(value))
    preview = node.cli(node.witness / "owner.token", "artifact-prepare-release", "--application", app_file,
                       "--delegation", allow["delegation_ref"]["resource_id"], "--key", key(),
                       "--signature", signature_file, source_file)
    assert preview["resource_refs"] == [app, source, signer, archive], preview
    accepted = node.call("Task.Submit", preview["request"])["task_id"]
    assert node.wait(accepted)["lifecycle"] == "WaitingApproval"
    assert_retained(node, source, archive, "pending version update")
    settled = node.wait(accepted, autoapprove=True)
    assert settled["lifecycle"] == "Succeeded", settled
    reference, = settled["result"]["artifact_refs"]
    receipt = node.call("Artifact.Read", {"reference": reference})
    evidence = node.call("Resource.Resolve", {"reference": receipt["stage_ref"]})["value"]
    assert evidence["source_artifact_ref"] == source and evidence["application_ref"] == app
    assert evidence["image_archive_ref"] == archive and evidence["image_capture_ref"] == capture, evidence
    release = node.call("Resource.Resolve", {"reference": receipt["release_ref"]})["value"]
    assert release["publisher_identity_ref"]["resource_id"] == publisher["principal_id"]
    specification = node.call("Resource.Resolve", {"reference": release["release_spec_ref"]})["value"]
    trust = specification["provenance"]
    assert trust["source_trust"] == "PublisherVerified" and all(ref in trust["source_refs"] for ref in (source, archive, capture))
    evidence = node.call("Resource.Resolve", {"reference": trust["trust_evidence_or_admission_policy_ref"]})["value"]
    assert evidence["signature"] == signature and evidence["artifact_ref"] == source
    print("Staged a real OCI release from a signed model session output", flush=True)
    assert_retained(node, source, archive, "staged version")
    return receipt["release_ref"], source, signer, archive


def retire_oci_release(node, app, release, artifact, archive, allow, uninstall=False):
    tool = "application.retire_release"

    def prepare(current):
        return node.prepare(tool, {"target": target(current), "release_ref": release, "reason": "deployment removed"},
                            delegation=allow["delegation_ref"]["resource_id"])

    if uninstall:
        try:
            node.call("Task.Submit", prepare(app))
        except Failure as denied:
            assert denied.code == "Conflict", denied
        else:
            raise AssertionError("retirement accepted a still-selected release")
        app = node.perform("application.uninstall", target(app))
    job = node.call("Task.Submit", prepare(app))["task_id"]
    settled = node.wait(job, autoapprove=True)
    assert settled["lifecycle"] == "Succeeded", settled
    reference, = settled["result"]["artifact_refs"]
    receipt = node.call("Artifact.Read", {"reference": reference})
    retired = node.call("Resource.Resolve", {"reference": receipt})["value"]
    assert retired["release_ref"] == release and retired["released_artifact_ref"] == artifact
    assert retired["released_archive_ref"] == archive
    for reference in (artifact, archive):
        node.call("Artifact.Withdraw", {"reference": reference, "reason": "retired local OCI release"})
        assert node.call("Artifact.Get", {"reference": reference})["availability"] == "Evicted"
    assert node.call("Resource.Resolve", {"reference": release})["reference"] == release
    print("Retired the " + ("uninstalled" if uninstall else "superseded") + " OCI release and reclaimed its source artifact", flush=True)


def check_log_isolation(node, app, owner, install_grant, container):
    token = owner["token"]
    app_id = app["resource_id"]
    tool = "application.logs"
    artifact_grant(node, owner["principal_id"], ["resource.read", "task.read", "event.read"], [app_id])

    def submit(grant):
        request = node.prepare(tool, target(app), token=token,
                               delegation=grant["delegation_ref"]["resource_id"])
        return node.call("Task.Submit", request, token)["task_id"]

    def read(grant):
        task = node.wait(submit(grant))
        assert task["lifecycle"] == "Succeeded", task
        reference, = task["result"]["artifact_refs"]
        return reference, node.call("Artifact.Read", {"reference": reference}, token)

    denied(node, "PermissionDenied", "Task.Prepare", {"tool": tool, "input": target(app), "key": key(),
        "delegation_id": install_grant["delegation_ref"]["resource_id"]}, token)
    allowed = artifact_grant(node, owner["principal_id"], ["task.submit", "task.read", tool], [app_id])
    live_ref, live = read(allowed)
    assert live["availability"] == "Available" and live["truncated"], live
    assert live["stdout"].endswith("probe ready\n") and len(live["stdout"].encode()) == 8192, live
    assert live["stderr"] == "private-tail stderr\n", live
    assert live["source"]["application_ref"] == app and live["source"]["container_id"] == container, live

    original = json.loads(node.config.read_text())
    socket = node.base / "logs-auth.sock"
    path = f"/containers/{container}/logs?".encode()
    node.stop()
    with ReceiptBarrier(socket, original.get("docker_socket", "/var/run/docker.sock"),
                        lambda request: b"GET " in request and path in request,
                        status=b" 200 ", forward=True) as barrier:
        serving = threading.Thread(target=barrier.serve_forever, daemon=True)
        serving.start()
        try:
            node.config.write_text(json.dumps(dict(original, docker_socket=str(socket))))
            node.start()
            in_flight = submit(allowed)
            assert barrier.confirmed.wait(12), "Docker did not return the live log tail"
            node.call("Grant.Revoke", target(allowed["grant_ref"]))
            barrier.release.set()
            rejected = node.wait(in_flight)
            assert rejected["lifecycle"] == "Failed" and rejected["conditions"][0]["type"] in (
                "PermissionDenied", "Fenced"), rejected
            assert rejected["result"]["artifact_refs"] == [], rejected
        finally:
            barrier.release.set()
            barrier.shutdown()
            serving.join(3)
            node.stop()
            node.config.write_text(json.dumps(original))
            node.start()

    denied(node, "PermissionDenied", "Artifact.Read", {"reference": live_ref}, token)
    tail_page = node.call("Resource.List", {"kind": "ApplicationLogTail"}, token)
    app = node.perform("application.stop", target(app))
    status = node.call("Resource.Get", {"id": app_id}, token)
    assert status["reference"] == app and "last_logs" not in status["value"], status
    assert "private-tail stderr" not in json.dumps(status), status
    private_id = "application-log-tail-" + status["value"]["last_stop_operation_id"]
    denied(node, "PermissionDenied", "Resource.Get", {"id": private_id}, token)
    assert node.call("Resource.Watch", {"cursor": tail_page["watch_start_cursor"]}, token)["events"] == []
    assert node.call("Resource.List", {"kind": "ApplicationLogTail"}, token)["items"] == []
    assert "private-tail stderr" not in json.dumps(node.perform("application.health", target(app)))
    node.stop()
    node.start()
    allowed = artifact_grant(node, owner["principal_id"], ["task.submit", "task.read", tool], [app_id])
    cached_ref, cached = read(allowed)
    assert cached["availability"] == "Available" and cached["truncated"], cached
    for field in ("stdout", "stderr", "source"):
        assert cached[field] == live[field], (field, cached[field], live[field])
    node.call("Grant.Revoke", target(allowed["grant_ref"]))
    for method in ("Artifact.Read", "Resource.Resolve"):
        denied(node, "PermissionDenied", method, {"reference": cached_ref}, token)
    denied(node, "PermissionDenied", "Artifact.ReadChunk", {"reference": cached_ref, "offset": 0, "length": 1}, token)
    print("Verified scoped running and retained logs, targeted revocation, and private stopped tail", flush=True)
    return app


def main(binaries, previous_binaries=None):
    node = Node(binaries)
    tag = "psios-acceptance:" + key()
    upgraded_tag = tag + "-upgrade"
    app_ids = []
    docker = ["docker", "--host", "unix:///var/run/docker.sock"]
    try:
        c = node.base / "probe.c"
        c.write_text('''#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(void) {
    const char *path=getenv("EPHEMERAL_ONLY") ? "/tmp/starts" : "/data/starts";
    FILE *f=fopen(path, "a");
    if (!f) return 2;
    fprintf(f,"%s\\n",getenv("TEST_NAME"));
    if (fclose(f)) return 3;
    if (getenv("TEST_LONG_LOG")) {
        for (int i=0; i<9000; i++) fputc('x',stdout);
        fputc('\\n',stdout);
        fputs("private-tail stderr\\n",stderr); fflush(stderr);
    }
    puts("probe ready"); fflush(stdout);
    for (;;) pause();
}
''')
        binary = node.base / "probe"
        subprocess.run(["cc", "-static", "-O2", str(c), "-o", str(binary)], check=True)
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            tar.add(binary, arcname="probe")
        image_id = subprocess.check_output(docker + ["import", "-", tag], input=archive.getvalue()).decode().strip()
        volume = node.perform("volume.create", {"name": "persistent-data"})
        spec = dict(image_id=image_id, name="acceptance", release_version="1.0.0", argv=["/probe"],
                    environment={"TEST_NAME": "first"}, license_expression="0BSD", volume=target(volume),
                    cpu_millicores=200, memory_max_bytes=33554432, pids_max=16, ephemeral_max_bytes=1048576)
        app, source_artifact, source_archive = install_from_artifact(node, spec)
        app_id = app["resource_id"]
        app_ids.append(app_id)
        data = node.state / "volumes" / volume["resource_id"] / "data"
        detached = data.with_name("test-unavailable")
        data.rename(detached)
        missing = node.wait(node.submit("application.start", target(app)), autoapprove=True)
        assert missing["lifecycle"] == "Failed" and not data.exists(), missing
        detached.rename(data)
        pending = node.submit("application.start", target(app))
        approval = node.get(node.wait(pending)["conditions"][0]["message"])["value"]
        plan = node.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
        assert [p["resource_ref"]["resource_id"] for p in plan["preconditions"]] == [app_id, volume["resource_id"]]
        volume = node.perform("volume.write", dict(**target(volume), name="changed", content="new revision"))["volume_ref"]
        node.approve(pending)
        invalidated = node.wait(pending)
        assert invalidated["outcome_knowledge"] == "NotStarted" and invalidated["conditions"][0]["type"] == "ApprovalInvalidated"
        assert not subprocess.check_output(docker + ["ps", "-aq", "--filter", "label=io.psios.application=" + app_id]).strip()
        app = node.perform("application.start", target(app))
        health = node.perform("application.health", target(app))
        assert health["process_running"] is True, health
        assert "probe ready" in node.perform("application.logs", target(app))["stdout"]
        cid = health["container_id"]
        inspected = json.loads(subprocess.check_output(docker + ["inspect", cid]))[0]
        assert inspected["HostConfig"]["NetworkMode"] == "none"
        assert inspected["HostConfig"]["ReadonlyRootfs"]
        assert inspected["HostConfig"]["Memory"] == spec["memory_max_bytes"]
        assert inspected["HostConfig"]["MemorySwap"] == spec["memory_max_bytes"]
        assert inspected["HostConfig"]["PidsLimit"] == 16
        if os.getenv("PSIOS_TEST_OCI_KERNEL_LIMITS"):
            check_kernel_limits(node.get(app_id)["value"]["last_observation"], docker, cid)
        volume = node.get(volume["resource_id"])["reference"]
        busy = node.wait(node.submit("backup.create", target(volume)))
        assert busy["lifecycle"] == "Failed" and busy["conditions"][0]["type"] == "Conflict", busy
        busy = node.wait(node.submit("volume.usage", {"target": target(volume)}))
        assert busy["lifecycle"] == "Failed" and busy["conditions"][0]["type"] == "Conflict", busy
        node.stop()
        node.start()
        assert node.perform("application.health", target(app))["container_id"] == cid
        app = node.perform("application.stop", target(app))
        volume = node.get(volume["resource_id"])["reference"]
        measured = node.perform("volume.usage", {"target": target(volume)})
        assert measured["files"] == 2 and measured["logical_file_bytes"] == str(len("new revisionfirst\n")), measured
        app = node.call("Application.Configure", dict(**target(app), environment={"TEST_NAME": "second"}))
        app = node.perform("application.start", target(app))
        app = node.perform("application.stop", target(app))
        previous_release = node.get(app_id)["resource"]["spec"]["release_ref"]
        upgraded_image = subprocess.check_output(docker + ["import", "--change", "LABEL test.release=2", "-", upgraded_tag],
            input=archive.getvalue()).decode().strip()
        assert upgraded_image != spec["image_id"]
        release = dict(spec, image_id=upgraded_image, release_version="2.0.0", environment={"TEST_NAME": "upgraded"})
        del release["volume"]
        staged, staged_artifact, signing_key, staged_archive = stage_from_artifact(node, app, release)
        assert node.get(app_id)["resource"]["spec"]["release_ref"] == previous_release
        with corrupted_archive(node, staged_archive):
            broken = node.wait(node.submit("application.select_release", dict(target=target(app), release_ref=staged)), autoapprove=True)
            assert broken["lifecycle"] == "Failed" and broken["conditions"][0]["type"] == "RecoveryRequired", broken
            assert node.get(app_id)["resource"]["spec"]["release_ref"] == previous_release
        agent = node.call("Agent.Register", {"name": "installer"})
        grant = node.call("Grant.Issue", dict(principal_id=agent["principal_id"], actions=["task.submit","task.read","application.install"],
            ttl_seconds=180, units=2, tool_targets=["profile-local"]))
        isolated = dict(release, name="isolated", environment={"TEST_NAME": "isolated", "EPHEMERAL_ONLY": "1", "TEST_LONG_LOG": "1"})
        request = node.prepare("application.install", isolated, key(), agent["token"], grant["delegation_ref"]["resource_id"])
        accepted = node.call("Task.Submit", request, agent["token"])
        installed = node.wait(accepted["task_id"], autoapprove=True)
        assert installed["lifecycle"] == "Succeeded", installed
        other = node.call("Artifact.Read", {"reference": installed["result"]["artifact_refs"][0]}, agent["token"])
        app_ids.append(other["resource_id"])
        try:
            node.call("Application.Configure", dict(**target(other), environment={"TEST_NAME":"outside-scope"}), agent["token"])
        except Failure as e:
            assert e.code == "PermissionDenied", e
        else:
            raise AssertionError("configuration bypassed the install grant target")
        foreign = node.wait(node.submit("application.select_release", dict(target=target(other), release_ref=staged)), autoapprove=True)
        assert foreign["conditions"][0]["type"] == "PreconditionFailed", foreign
        other = node.perform("application.start", target(other))
        ephemeral_cid = node.perform("application.health", target(other))["container_id"]
        ephemeral = json.loads(subprocess.check_output(docker + ["inspect", ephemeral_cid]))[0]
        assert ephemeral["Mounts"] == [], ephemeral["Mounts"]
        other = check_log_isolation(node, other, agent, grant, ephemeral_cid)
        node.perform("application.uninstall", target(other))
        app = node.perform("application.select_release", dict(target=target(app), release_ref=staged))
        with corrupted_archive(node, staged_archive):
            broken = node.wait(node.submit("application.start", target(app)), autoapprove=True)
            assert broken["lifecycle"] == "Failed" and broken["conditions"][0]["type"] == "RecoveryRequired", broken
            assert not subprocess.check_output(docker + ["ps", "-aq", "--filter", "label=io.psios.application=" + app_id]).strip()
        fence_after_container_creation(node, docker, app, staged_archive)
        app = node.perform("application.start", target(app))
        changed_cid = node.perform("application.health", target(app))["container_id"]
        assert json.loads(subprocess.check_output(docker + ["inspect", changed_cid]))[0]["Image"] == upgraded_image
        node.call("Artifact.PublisherKey.Revoke", {**target(signing_key), "reason": "end of publisher trust"})
        subprocess.run(docker + ["kill", changed_cid], check=True, stdout=subprocess.DEVNULL)
        app = node.perform("application.refresh", target(app))
        observed = node.get(app_id)["value"]
        assert observed["desired_state"] == "Running" and observed["runtime_state"] == "Exited"
        assert node.get(observed["instance_ref"]["resource_id"])["value"]["lifecycle"] == "Exited"
        node.stop()
        node.start()
        assert node.get(app_id)["value"]["readiness"] == "NotReady"
        app = node.perform("application.stop", target(app))
        restart_denied = node.wait(node.submit("application.start", target(app)), autoapprove=True)
        assert (restart_denied["lifecycle"], restart_denied["outcome_knowledge"], restart_denied["conditions"][0]["type"]) == (
            "Failed", "NotStarted", "PermissionDenied"), restart_denied
        app = node.perform("application.select_release", dict(target=target(app), release_ref=previous_release))
        app = lose_start_receipt(node, app, volume, previous_binaries)
        reverted = node.perform("application.health", target(app))["container_id"]
        assert json.loads(subprocess.check_output(docker + ["inspect", reverted]))[0]["Image"] == image_id
        app = lose_stop_receipt(node, app, reverted, volume)
        unavailable = node.perform("application.logs", target(app))
        assert unavailable["availability"] == "Unavailable" and not unavailable["stdout"], unavailable
        recovery_volume = node.perform("volume.create", {"name": "stop-continuation-data"})
        cleanup_spec = dict(spec, name="stop-continuation", environment={"TEST_NAME": "continuation"},
                            volume=target(recovery_volume))
        cleanup_app = node.perform("application.install", cleanup_spec)
        app_ids.append(cleanup_app["resource_id"])
        for lose_removal in (False, True):
            cleanup_app = node.perform("application.start", target(cleanup_app))
            recovery_volume = node.get(recovery_volume["resource_id"])["reference"]
            cleanup_app = continue_after_lost_stop(node, cleanup_app, recovery_volume, lose_removal=lose_removal)
        node.perform("application.uninstall", target(cleanup_app))
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        artifact_grant(node, "local-owner", ["artifact.withdraw", "artifact.read"], [node_id])
        retirement = artifact_grant(node, "local-owner", ["task.submit", "task.read", "application.retire_release"],
                                    [app_id, staged["resource_id"], previous_release["resource_id"]])
        artifact_grant(node, "local-owner", ["application.releases"], [app_id])
        cursor_page = node.cli(node.witness / "owner.token", "application-releases", app_id, "--limit", "1")
        assert cursor_page["items"][0]["state"] == "Selected", cursor_page
        assert cursor_page["selected_release_ref"] == previous_release, cursor_page
        retire_oci_release(node, app, staged, staged_artifact, staged_archive, retirement)
        retire_oci_release(node, app, previous_release, source_artifact, source_archive, retirement, uninstall=True)
        terminal_page = node.call("Application.ListReleases", {"application_id": app_id, "cursor": cursor_page["next_cursor"]})
        assert terminal_page["items"][0]["release_ref"] == staged
        assert terminal_page["items"][0]["state"] == "Unselected" and terminal_page["application_ref"] == app, terminal_page
        fresh = node.call("Application.ListReleases", {"application_id": app_id})
        assert fresh["application_lifecycle"] == "Uninstalled" and fresh["selected_release_ref"] is None
        assert [item["state"] for item in fresh["items"]] == ["Retired", "Retired"], fresh
        print("Queried real OCI release history before and after retirement and uninstall", flush=True)
        volume = node.get(volume["resource_id"])["reference"]
        assert node.perform("volume.read", dict(**target(volume), name="starts"))["content"] == "first\nsecond\nupgraded\nfirst\n"
        backup = node.perform("backup.create", target(volume))
        restored = node.perform("backup.restore", target(backup))
        assert restored["resource_id"] != volume["resource_id"]
        assert node.perform("volume.read", dict(**target(restored), name="starts"))["content"] == "first\nsecond\nupgraded\nfirst\n"
        node.perform("volume.activate", target(restored))
        verify_restart(node, docker, spec, app_ids)
        verify_image_health(node, docker)
        node.stop()
        subprocess.run(node.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=15)
        print("PASS live OCI lifecycle, scoped restart, isolated execution, release rollback, external exit, receipt recovery, volume holds and verified backup restore")
    finally:
        for app_id in app_ids:
            ids = subprocess.check_output(docker + ["ps", "-aq", "--filter", "label=io.psios.application=" + app_id], text=True).split()
            for cid in ids:
                subprocess.run(docker + ["rm", "-f", cid], check=True, stdout=subprocess.DEVNULL)
        for owned_tag in [tag, upgraded_tag]:
            if subprocess.run(docker + ["image", "inspect", owned_tag], capture_output=True).returncode == 0:
                subprocess.run(docker + ["image", "rm", owned_tag], check=True, stdout=subprocess.DEVNULL)
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", default="target/debug")
    parser.add_argument("--previous-bin-dir", help="same database format, prior OCI evidence policy")
    args = parser.parse_args()
    main(args.bin_dir, args.previous_bin_dir)
