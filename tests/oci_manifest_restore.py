"""Capture and restore a real tagless Docker containerd manifest and its gzip layer."""

import argparse
import hashlib
import io
import json
from pathlib import Path
import signal
import subprocess
import tarfile
import time

from oci_capacity import install_and_restore, read_archive, verify_authority
from oci_capture import DOCKER, TOOL, checkpoint_archive, grant
from oci_restore import current_image
from support import Node, key, target


def fixture_layer(directory, tag):
    source = directory / "manifest-probe.c"
    probe = directory / "manifest-probe"
    source.write_text('''#include <stdio.h>
#include <unistd.h>
int main(void) {
    puts("restored manifest probe ready");
    fflush(stdout);
    for (;;) pause();
}
''')
    subprocess.run(["cc", "-static", "-O2", source, "-o", probe], check=True, timeout=30)
    contents = (tag + "\n").encode() * 4096
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w", format=tarfile.USTAR_FORMAT) as layer:
        member = tarfile.TarInfo("manifest-fixture")
        member.size = len(contents)
        layer.addfile(member, io.BytesIO(contents))
        layer.add(probe, arcname="probe")
    return archive.getvalue()


def import_fixture(directory, tag):
    return subprocess.check_output([*DOCKER, "image", "import", "-", tag], input=fixture_layer(directory, tag),
                                   timeout=20).decode().strip()


def capture(node, image, engine, delegation):
    request = node.prepare(TOOL, {"image_id": image, "expected_engine_id": engine}, delegation=delegation)
    finished = node.wait(node.call("Task.Submit", request)["task_id"], autoapprove=True)
    assert (finished["lifecycle"], finished["outcome_knowledge"]) == ("Succeeded", "Known"), finished
    receipt = node.call("Artifact.Read", {"reference": finished["result"]["artifact_refs"][0]})
    proof = node.call("Resource.Resolve", {"reference": receipt["capture_ref"]})["value"]
    assert proof["artifact_ref"] == receipt["artifact_ref"]
    assert proof["restore_compatibility"] == receipt["restore_compatibility"], (proof, receipt)
    return receipt, proof


def owned_containers(app):
    return subprocess.check_output([*DOCKER, "ps", "-aq", "--no-trunc", "--filter",
        "label=io.psios.application=" + app["resource_id"]], text=True, timeout=15).split()


def verify_start_and_stop(node, app):
    node.start()
    started = node.perform("application.start", target(app))
    health = node.perform("application.health", target(started))
    assert health["process_running"] and owned_containers(app) == [health["container_id"]], health
    deadline = time.monotonic() + 10
    while True:
        logs = node.perform("application.logs", target(started))
        if "restored manifest probe ready" in logs["stdout"]:
            break
        assert time.monotonic() < deadline, logs
        time.sleep(0.1)
    stopped = node.perform("application.stop", target(started))
    health = node.perform("application.health", target(stopped))
    assert not health["process_running"], health
    node.stop()
    verify_authority(node)


def main(binaries, previous=None):
    info = json.loads(subprocess.check_output([*DOCKER, "info", "--format", "{{json .}}"], timeout=15))
    assert ["driver-type", "io.containerd.snapshotter.v1"] in info["DriverStatus"], info["DriverStatus"]
    node = Node(previous or binaries)
    tag = "psios-manifest-restore:" + key()
    image = None
    app = None
    try:
        image = import_fixture(node.base, tag)
        layers = current_image(image)["RootFS"]["Layers"]
        policy = grant(node, "local-owner", "task.submit", "task.read", TOOL,
                       "artifact.upload", "artifact.read", "artifact.withdraw")
        delegation = policy["delegation_ref"]["resource_id"]
        if previous is not None:
            old_receipt, old_proof = capture(node, image, info["ID"], delegation)
            assert old_proof["restore_compatibility"] == {"status": "Unsupported", "reason": "ManifestIdentity"}
            node.stop(signal.SIGKILL)
            original = node.daemon[:]
            old = verify_authority(node)
            assert (old["database_format"], old["migration_required"]) == (132, False), old
            node.binaries = binaries.absolute()
            node.daemon[0] = str(node.binaries / "psiosd")
            staged = verify_authority(node)
            assert (staged["node_id"], staged["database_format"], staged["migration_required"]) == (
                old["node_id"], 132, True), staged
            node.start()
            assert node.call("Resource.Resolve", {"reference": old_receipt["capture_ref"]})["value"] == old_proof
            node.call("Artifact.Withdraw", {"reference": old_receipt["artifact_ref"], "reason": "replace old capture"})

        receipt, proof = capture(node, image, info["ID"], delegation)
        body = read_archive(node, receipt["artifact_ref"])
        with tarfile.open(fileobj=io.BytesIO(body), mode="r:") as archive:
            descriptor, = json.load(archive.extractfile("index.json"))["manifests"]
            assert descriptor["digest"] == image, descriptor
            manifest = json.load(archive.extractfile("blobs/" + image.replace(":", "/", 1)))
            assert manifest["layers"][0]["mediaType"].endswith("tar+gzip"), manifest
            cfg = manifest["config"]["digest"]
            data = archive.extractfile("blobs/" + cfg.replace(":", "/", 1)).read()
            assert "sha256:" + hashlib.sha256(data).hexdigest() == cfg and image != cfg
        assert proof["image"]["diff_ids"] == layers
        assert proof["restore_compatibility"] == {"status": "Verified", "profile": "DockerOciTaglessSaveV1",
                                                    "config_digest": cfg}, proof
        checkpoint_archive(node, receipt["artifact_ref"], body)
        app = install_and_restore(node, tag, image, info["ID"], receipt["artifact_ref"], layers, argv=["/probe"])
        verify_start_and_stop(node, app)
        checked = "PASS signed gzip-layer OCI restore, exact manifest and DiffID, application start and stop, detached checkpoint"
        if previous is not None:
            upgraded = verify_authority(node)
            assert upgraded["node_id"] == old["node_id"] and upgraded["database_format"] >= 133, upgraded
            assert not upgraded["migration_required"], upgraded
            refused = subprocess.run([*original, "serve"], capture_output=True, text=True, timeout=15)
            assert refused.returncode != 0 and "SchemaVersionUnsupported" in refused.stderr, refused
            checked += " and writer fence"
        print(checked)
    finally:
        node.close()
        if app is not None:
            for container in owned_containers(app):
                subprocess.run([*DOCKER, "rm", "-f", container], check=True, capture_output=True, timeout=15)
        for reference in (tag, image):
            if reference:
                subprocess.run([*DOCKER, "image", "rm", reference], capture_output=True, timeout=15)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default=Path("target/debug"), type=Path)
    parser.add_argument("--previous-bin-dir", type=Path)
    args = parser.parse_args()
    main(args.bin_dir, args.previous_bin_dir)
