"""Verify an installed, digest-addressed OCI application with an actual zstd layer."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import signal
import subprocess
import tarfile

from oci_capacity import install_and_restore, read_archive, verify_authority
from oci_capture import DOCKER, TOOL, checkpoint_archive, grant
from oci_manifest_restore import capture, fixture_layer, owned_containers, verify_start_and_stop
from oci_restore import current_image
from support import Node, key


OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
OCI_CONFIG = "application/vnd.oci.image.config.v1+json"
OCI_ZSTD_LAYER = "application/vnd.oci.image.layer.v1.tar+zstd"


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def descriptor(content, media):
    return {"size": len(content), "mediaType": media, "digest": digest(content)}


def blob(reference):
    return "blobs/" + reference["digest"].replace(":", "/", 1)


def serialized(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def source_image(directory):
    expanded = fixture_layer(directory, "psios-zstd-restore-" + key())
    compressed = subprocess.check_output(["zstd", "-q", "-c", "-1"], input=expanded, timeout=20)
    layer = descriptor(compressed, OCI_ZSTD_LAYER)
    diffid = digest(expanded)
    config = serialized({"architecture": "amd64", "os": "linux", "config": {}, "history": [],
                         "rootfs": {"type": "layers", "diff_ids": [diffid]}})
    config_ref = descriptor(config, OCI_CONFIG)
    manifest = serialized({"schemaVersion": 2, "mediaType": OCI_MANIFEST, "config": config_ref, "layers": [layer]})
    image = descriptor(manifest, OCI_MANIFEST)
    index = serialized({"schemaVersion": 2, "manifests": [image]})
    contents = {"oci-layout": b'{"imageLayoutVersion":"1.0.0"}', "index.json": index, blob(image): manifest,
                blob(config_ref): config, blob(layer): compressed}
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in contents.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return image, diffid, config_ref, layer, output.getvalue()


def main(binaries, previous=None):
    info = json.loads(subprocess.check_output([*DOCKER, "info", "--format", "{{json .}}"], timeout=15))
    assert ["driver-type", "io.containerd.snapshotter.v1"] in info["DriverStatus"], info["DriverStatus"]
    node = Node(previous or binaries)
    image_id = None
    application = None
    try:
        image, diffid, config, layer, source = source_image(node.base)
        image_id = image["digest"]
        assert current_image(image_id) is None
        subprocess.run([*DOCKER, "image", "load", "--quiet"], input=source, check=True, timeout=25)
        inspected = current_image(image_id)
        assert inspected["Id"] == image_id and inspected["RootFS"]["Layers"] == [diffid] and not inspected["RepoTags"], inspected
        policy = grant(node, "local-owner", "task.submit", "task.read", "artifact.read",
                       "artifact.upload", "artifact.withdraw", TOOL)
        delegation = policy["delegation_ref"]["resource_id"]
        if previous is not None:
            old_receipt, old_proof = capture(node, image_id, info["ID"], delegation)
            assert old_proof["restore_compatibility"] == {"status": "Unsupported", "reason": "ManifestIdentity"}, old_proof
            node.stop(signal.SIGKILL)
            original = node.daemon[:]
            old = verify_authority(node)
            assert (old["database_format"], old["migration_required"]) == (135, False), old
            node.binaries = binaries.absolute()
            node.daemon[0] = str(node.binaries / "psiosd")
            before = verify_authority(node)
            assert (before["node_id"], before["database_format"], before["migration_required"]) == (
                old["node_id"], 135, True), before
            node.start()
            assert node.call("Resource.Resolve", {"reference": old_receipt["capture_ref"]})["value"] == old_proof

        receipt, proof = capture(node, image_id, info["ID"], delegation)
        assert proof["image"]["diff_ids"] == [diffid], proof
        assert proof["restore_compatibility"] == {"status": "Verified", "profile": "DockerOciTaglessSaveV1",
                                                    "config_digest": config["digest"]}, proof
        body = read_archive(node, receipt["artifact_ref"])
        with tarfile.open(fileobj=io.BytesIO(body), mode="r:") as archive:
            original_manifest = json.load(archive.extractfile(blob(image)))
            assert original_manifest["layers"] == [layer], original_manifest
            assert digest(archive.extractfile(blob(layer)).read()) == layer["digest"]
        checkpoint_archive(node, receipt["artifact_ref"], body)
        application = install_and_restore(node, image_id, image_id, info["ID"], receipt["artifact_ref"], [diffid],
                                          argv=["/probe"])
        verify_start_and_stop(node, application)
        if previous is not None:
            after = verify_authority(node)
            assert (after["node_id"], after["database_format"], after["migration_required"]) == (old["node_id"], 136, False), after
            refused = subprocess.run([*original, "serve"], capture_output=True, text=True, timeout=15)
            assert refused.returncode != 0 and "SchemaVersionUnsupported" in refused.stderr, refused
        print("PASS real signed zstd OCI checkpoint, original manifest and DiffID, application execution and writer fence")
    finally:
        node.close()
        if application is not None:
            for container in owned_containers(application):
                subprocess.run([*DOCKER, "rm", "-f", container], check=True, capture_output=True, timeout=15)
        if image_id is not None:
            subprocess.run([*DOCKER, "image", "rm", image_id], capture_output=True, timeout=15)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default=Path("target/debug"), type=Path)
    parser.add_argument("--previous-bin-dir", type=Path)
    args = parser.parse_args()
    main(args.bin_dir, args.previous_bin_dir)
