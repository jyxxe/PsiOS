"""Explicit live-engine acceptance. Builds a unique scratch image without a registry or network."""
import argparse
import io
import json
import os
import subprocess
import tarfile

from support import Failure, Node, key, target
from oci_recovery import check_kernel_limits, lose_start_receipt, lose_stop_receipt
from oci_restart import verify_restart
from test_artifact_install import TOOL as ARTIFACT_INSTALL, grant as artifact_grant, prepare as prepare_artifact, publish_install


def install_from_artifact(node, specification):
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    artifact_grant(node, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
    artifact = publish_install(node, specification)
    allow = artifact_grant(node, "local-owner", ["task.submit", "task.read", ARTIFACT_INSTALL],
                          [specification["volume"]["id"], artifact["resource_id"]])
    ready = prepare_artifact(node, artifact, allow)
    task_id = node.call("Task.Submit", ready["request"])["task_id"]
    assert node.wait(task_id)["lifecycle"] == "WaitingApproval"

    def assert_retained(state):
        try:
            node.call("Artifact.Withdraw", {"reference": artifact, "reason": state})
        except Failure as denied:
            assert denied.code == "Conflict", denied
        else:
            raise AssertionError("withdrawal bypassed the installation pin")

    assert_retained("pending installation")
    completed = node.wait(task_id, autoapprove=True)
    assert completed["lifecycle"] == "Succeeded", completed
    assert_retained("retained release")
    reference, = completed["result"]["artifact_refs"]
    receipt = node.call("Artifact.Read", {"reference": reference})
    assert receipt["source_artifact_ref"] == artifact, receipt
    evidence = node.call("Resource.Resolve", {"reference": receipt["installation_ref"]})["value"]
    assert evidence["deployment_ref"] == receipt["deployment_ref"]
    assert receipt["operation_id"] == completed["operation_refs"][0]["resource_id"]
    assert node.get(receipt["application_ref"]["resource_id"])["value"]["lifecycle"] == "Stopped"
    print("Installed a real local OCI image from an approved uploaded declaration", flush=True)
    return receipt["application_ref"], artifact


def stage_from_artifact(node, app, specification):
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    artifact_grant(node, "local-owner", ["artifact.upload", "artifact.read", "artifact.withdraw"], [node_id])
    source = publish_install(node, specification)
    allow = artifact_grant(node, "local-owner", ["task.submit", "task.read", "application.stage_artifact_release"],
                          [app["resource_id"], source["resource_id"]])
    app_file = node.base / "stage-application.json"
    source_file = node.base / "stage-artifact.json"
    app_file.write_text(json.dumps(app))
    source_file.write_text(json.dumps(source))
    preview = node.cli(node.witness / "owner.token", "artifact-prepare-release", "--application", app_file,
                       "--delegation", allow["delegation_ref"]["resource_id"], "--key", key(), source_file)
    assert preview["resource_refs"] == [app, source], preview
    accepted = node.call("Task.Submit", preview["request"])["task_id"]
    assert node.wait(accepted)["lifecycle"] == "WaitingApproval"
    settled = node.wait(accepted, autoapprove=True)
    assert settled["lifecycle"] == "Succeeded", settled
    reference, = settled["result"]["artifact_refs"]
    receipt = node.call("Artifact.Read", {"reference": reference})
    evidence = node.call("Resource.Resolve", {"reference": receipt["stage_ref"]})["value"]
    assert evidence["source_artifact_ref"] == source and evidence["application_ref"] == app
    print("Staged a newer real OCI release through the CLI and approved artifact workflow", flush=True)
    return receipt["release_ref"], source


def retire_oci_release(node, app, release, artifact, allow, uninstall=False):
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
    node.call("Artifact.Withdraw", {"reference": artifact, "reason": "retired local OCI release"})
    assert node.call("Artifact.Get", {"reference": artifact})["availability"] == "Evicted"
    assert node.call("Resource.Resolve", {"reference": release})["reference"] == release
    print("Retired the " + ("uninstalled" if uninstall else "superseded") + " OCI release and reclaimed its source artifact", flush=True)


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
        app, source_artifact = install_from_artifact(node, spec)
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
        staged, staged_artifact = stage_from_artifact(node, app, release)
        assert node.get(app_id)["resource"]["spec"]["release_ref"] == previous_release
        agent = node.call("Agent.Register", {"name": "installer"})
        grant = node.call("Grant.Issue", dict(principal_id=agent["principal_id"], actions=["task.submit","task.read","application.install"],
            ttl_seconds=180, units=2, tool_targets=["profile-local"]))
        isolated = dict(release, name="isolated", environment={"TEST_NAME": "isolated", "EPHEMERAL_ONLY": "1"})
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
        assert "probe ready" in node.perform("application.logs", target(other))["stdout"]
        ephemeral = json.loads(subprocess.check_output(docker + ["inspect", ephemeral_cid]))[0]
        assert ephemeral["Mounts"] == [], ephemeral["Mounts"]
        other = node.perform("application.stop", target(other))
        node.perform("application.uninstall", target(other))
        app = node.perform("application.select_release", dict(target=target(app), release_ref=staged))
        app = node.perform("application.start", target(app))
        changed_cid = node.perform("application.health", target(app))["container_id"]
        assert json.loads(subprocess.check_output(docker + ["inspect", changed_cid]))[0]["Image"] == upgraded_image
        subprocess.run(docker + ["kill", changed_cid], check=True, stdout=subprocess.DEVNULL)
        app = node.perform("application.refresh", target(app))
        observed = node.get(app_id)["value"]
        assert observed["desired_state"] == "Running" and observed["runtime_state"] == "Exited"
        assert node.get(observed["instance_ref"]["resource_id"])["value"]["lifecycle"] == "Exited"
        node.stop()
        node.start()
        assert node.get(app_id)["value"]["readiness"] == "NotReady"
        app = node.perform("application.stop", target(app))
        app = node.perform("application.select_release", dict(target=target(app), release_ref=previous_release))
        app = lose_start_receipt(node, app, volume, previous_binaries)
        reverted = node.perform("application.health", target(app))["container_id"]
        assert json.loads(subprocess.check_output(docker + ["inspect", reverted]))[0]["Image"] == image_id
        app = lose_stop_receipt(node, app, reverted, volume)
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        artifact_grant(node, "local-owner", ["artifact.withdraw", "artifact.read"], [node_id])
        retirement = artifact_grant(node, "local-owner", ["task.submit", "task.read", "application.retire_release"],
                                    [app_id, staged["resource_id"], previous_release["resource_id"]])
        artifact_grant(node, "local-owner", ["application.releases"], [app_id])
        cursor_page = node.cli(node.witness / "owner.token", "application-releases", app_id, "--limit", "1")
        assert cursor_page["items"][0]["state"] == "Selected", cursor_page
        assert cursor_page["selected_release_ref"] == previous_release, cursor_page
        retire_oci_release(node, app, staged, staged_artifact, retirement)
        retire_oci_release(node, app, previous_release, source_artifact, retirement, uninstall=True)
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
