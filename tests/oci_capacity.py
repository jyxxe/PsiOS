"""Live Docker acceptance for larger, explicitly approved OCI archives."""
import argparse
import base64
from contextlib import closing
import hashlib
import json
from pathlib import Path
import signal
import sqlite3
import subprocess
import time

from oci import ARCHIVED_MEDIA, archived_declaration
from oci_capture import CHUNK, DOCKER, MAX, TOOL, assert_failed, checkpoint_archive, denied, grant, import_fixture, no_lost_publication
from oci_restore import RESTORE, current_image, rm_image
from support import Node, key, target
from test_artifact_ingress import chunk, upload_request
from test_artifact_install import TOOL as INSTALL, grant as scoped_grant
from test_oci_publisher import local_signature


MIB = 1024 * 1024
LIMIT = 64 * MIB


def configure(node, config):
    node.stop()
    node.config.write_text(json.dumps(config))
    node.start()


def read_archive(node, reference):
    data = bytearray()
    while True:
        result = node.call("Artifact.ReadChunk", {"reference": reference, "offset": len(data), "length": CHUNK})
        data.extend(base64.b64decode(result["data_base64"], validate=True))
        if result["next_offset"] is None:
            return data
        assert result["next_offset"] == len(data) <= LIMIT, result["next_offset"]


def install_and_restore(node, tag, image, engine, archive, diff_ids):
    install = {"image_id": image, "argv": ["/unused"], "name": "large-archive-fixture",
               "release_version": "1.0.0", "license_expression": "0BSD", "cpu_millicores": 100,
               "memory_max_bytes": 33554432, "pids_max": 8, "ephemeral_max_bytes": 1048576}
    body = archived_declaration(install, archive).encode()
    stage = node.call("Artifact.Stage", upload_request(body, declared_media_type=ARCHIVED_MEDIA))
    node.call("Artifact.Stage.PutChunk", chunk(stage, 0, body))
    source = node.call("Artifact.Publish", {"stage_id": stage["stage_id"]})["artifact_ref"]
    signature, _, _ = local_signature(node, source)
    signer = signature["key_ref"]
    roles = [ref["resource_id"] for ref in (source, signer, archive)]
    grant = scoped_grant(node, "local-owner", ["task.submit", "task.read", INSTALL], ["profile-local", *roles])
    request = node.call("Application.PrepareArtifactInstall", {"reference": source, "signature": signature,
        "key": key(), "delegation_id": grant["delegation_ref"]["resource_id"]})
    assert request["image_archive_ref"] == archive, request
    completed = node.wait(node.call("Task.Submit", request["request"])["task_id"], autoapprove=True)
    assert completed["lifecycle"] == "Succeeded", completed
    installed = node.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
    app, release = installed["application_ref"], installed["release_ref"]
    assert installed["image_archive_ref"] == archive, installed
    denied(node, "Conflict", "Artifact.Withdraw", {"reference": archive, "reason": "release pins exact large archive"})

    rm_image(tag)
    assert current_image(image) is None
    authority = node.call("Node.Get", {})["reference"]["resource_id"]
    scope = [app["resource_id"], release["resource_id"], *roles, authority]
    permit = scoped_grant(node, "local-owner", ["task.submit", "task.read", RESTORE], scope)
    request = node.prepare(RESTORE, {"target": target(app), "release_ref": release, "image_archive_ref": archive,
        "publisher_key_ref": signer, "expected_engine_id": engine}, delegation=permit["delegation_ref"]["resource_id"])
    restored = node.wait(node.call("Task.Submit", request)["task_id"], autoapprove=True)
    assert restored["lifecycle"] == "Succeeded", restored
    receipt = node.call("Artifact.Read", {"reference": restored["result"]["artifact_refs"][0]})
    assert receipt["image_archive_ref"] == archive and receipt["observation"]["image_diff_ids"] == diff_ids, receipt
    loaded = current_image(image)
    assert loaded["Id"] == image and not loaded["RepoTags"] and loaded["RootFS"]["Layers"] == diff_ids, loaded
    node.stop()
    subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=30)


def verify_authority(node):
    return json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=30))


def verify_disposal_charge(node, reference, original_size):
    size = int(node.call("Artifact.Get", {"reference": reference})["resource"]["spec"]["size_bytes"])
    node.stop()
    with closing(sqlite3.connect(node.state / "state.db")) as database, database:
        database.execute("""CREATE TRIGGER block_archive_disposition BEFORE UPDATE OF disposed ON oci_archive_blobs
                            BEGIN SELECT RAISE(ABORT, 'test OCI archive disposition loss'); END""")
    node.start()
    node.call("Artifact.Withdraw", {"reference": reference, "reason": "verify deferred archive reclamation"})
    before = node.call("Artifact.UploadCapacity", {})["principal"]
    assert before["oci_archive_retained_bytes"] == original_size, before
    assert before["oci_archive_pending_disposal_bytes"] == size, before
    node.stop(signal.SIGKILL)
    with closing(sqlite3.connect(node.state / "state.db")) as database, database:
        archived = database.execute("SELECT disposed FROM oci_archive_blobs WHERE artifact=?",
                                    (reference["resource_id"],)).fetchone()
        assert archived == (0,), archived
        database.execute("DROP TRIGGER block_archive_disposition")
    node.start()
    deadline = time.monotonic() + 15
    while True:
        after = node.call("Artifact.UploadCapacity", {})["principal"]
        if after["oci_archive_pending_disposal_bytes"] == 0:
            assert after["oci_archive_retained_bytes"] == original_size, after
            break
        assert time.monotonic() < deadline, after
        time.sleep(0.05)
    assert not (node.state / "oci-archives" / reference["resource_id"]).exists()


def main(binaries, previous_binaries=None):
    node = Node(previous_binaries or binaries)
    tag = "psios-large-capture:" + key()
    image = None
    previous = node.daemon[:]
    try:
        if previous_binaries is not None:
            node.stop(signal.SIGKILL)
            old = verify_authority(node)
            assert (old["database_format"], old["migration_required"]) == (131, False), old
            node.binaries = binaries.absolute()
            node.daemon[0] = str(node.binaries / "psiosd")
            staged = verify_authority(node)
            assert (staged["node_id"], staged["database_format"], staged["migration_required"]) == (
                old["node_id"], 131, True), staged
            node.start()
        image = import_fixture(tag, 40 * MIB)
        before = current_image(image)
        assert before["Id"] == image, before
        diff_ids = before["RootFS"]["Layers"]
        engine = subprocess.check_output(DOCKER + ["info", "--format", "{{.ID}}"], text=True, timeout=15).strip()
        grant(node, "local-owner", "artifact.upload", "artifact.read", "artifact.withdraw")
        permit = grant(node, "local-owner", "task.submit", "task.read", TOOL)
        flags = ["application-prepare-image-capture", "--image-id", image, "--engine-id", engine,
                 "--delegation", permit["delegation_ref"]["resource_id"]]

        def submit(*limit):
            request = node.cli(node.witness / "owner.token", *flags, "--key", key(), *limit)
            task = node.call("Task.Submit", request)["task_id"]
            waiting = node.wait(task)
            assert waiting["lifecycle"] == "WaitingApproval", waiting
            approval = node.get(waiting["conditions"][0]["message"])["value"]
            plan = node.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
            return task, node.call("Resource.Resolve", {"reference": plan["purpose_ref"]})["value"]

        config = json.loads(node.config.read_text())
        default, input_value = submit()
        assert input_value == {"image_id": image, "expected_engine_id": engine}, input_value
        configure(node, dict(config, oci_capture_max_bytes=LIMIT, oci_capture_staging_max_bytes=LIMIT,
                             oci_archive_storage={"principal_bytes": LIMIT, "node_bytes": 96 * MIB}))
        staging = node.call("Node.Get", {})["oci_capture_staging"]
        assert staging == {"max_bytes": LIMIT, "reserved_bytes": 0}, staging
        assert_failed(node.wait(default, autoapprove=True), "QuotaExceeded")
        no_lost_publication(node, 0)

        task, input_value = submit("--max-archive-bytes", str(LIMIT))
        assert input_value == {"image_id": image, "expected_engine_id": engine, "max_archive_bytes": LIMIT}, input_value
        completed = node.wait(task, autoapprove=True)
        assert completed["lifecycle"] == "Succeeded", completed
        receipt = node.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
        archive = receipt["artifact_ref"]
        proof = node.call("Resource.Resolve", {"reference": receipt["capture_ref"]})["value"]
        assert receipt["max_archive_bytes"] == proof["max_archive_bytes"] == LIMIT, (receipt, proof)
        assert proof["image"]["diff_ids"] == diff_ids and proof["restore_compatibility"]["status"] == "Verified", proof
        assert node.call("Artifact.Get", {"reference": archive})["resource"]["spec"]["content_layout"] == "ManagedOciArchiveV1"
        no_lost_publication(node, 1)
        size = int(proof["size_bytes"])
        assert 32 * MIB < size <= LIMIT, proof
        capacity = node.call("Artifact.UploadCapacity", {})
        assert capacity["principal"]["retained_bytes"] == 0
        assert capacity["principal"]["oci_archive_retained_bytes"] == size
        assert capacity["principal"]["oci_archive_byte_limit"] == LIMIT
        assert capacity["node"]["oci_archive_byte_limit"] == 96 * MIB

        task, _ = submit("--max-archive-bytes", str(LIMIT))
        assert_failed(node.wait(task, autoapprove=True), "QuotaExceeded")
        no_lost_publication(node, 1)
        assert node.call("Artifact.UploadCapacity", {}) == capacity

        reserve = {"size_bytes": MAX, "ttl_seconds": 300, "declared_media_type": "application/octet-stream",
                   "digest": "sha256:" + "3" * 64}
        reservations = [node.call("Artifact.Stage", {**reserve, "key": key()}) for _ in range(4)]
        denied(node, "QuotaExceeded", "Artifact.Stage", {**reserve, "key": key()})
        raw_full = node.call("Artifact.UploadCapacity", {})["principal"]
        assert raw_full["reserved_bytes"] == raw_full["byte_limit"] == 32 * MIB
        assert raw_full["oci_archive_retained_bytes"] == size
        configure(node, dict(config, oci_capture_max_bytes=LIMIT, oci_capture_staging_max_bytes=LIMIT,
                             oci_archive_storage={"principal_bytes": 96 * MIB, "node_bytes": 128 * MIB}))
        queued, _ = submit("--max-archive-bytes", str(LIMIT))
        second = node.wait(queued, autoapprove=True)
        assert second["lifecycle"] == "Succeeded", second
        assert node.call("Artifact.UploadCapacity", {})["principal"]["reserved_bytes"] == 32 * MIB
        discard = node.call("Artifact.Read", {"reference": second["result"]["artifact_refs"][0]})["artifact_ref"]
        assert discard["resource_id"] != archive["resource_id"], discard
        verify_disposal_charge(node, discard, size)
        for stage in reservations:
            node.call("Artifact.Stage.Discard", {"stage_id": stage["stage_id"]})

        configure(node, config)
        retained = node.call("Artifact.UploadCapacity", {})["principal"]
        assert retained["oci_archive_retained_bytes"] == size > retained["oci_archive_byte_limit"]
        data = read_archive(node, archive)
        assert len(data) == size, proof
        assert "sha256:" + hashlib.sha256(data).hexdigest() == archive["digest"] == proof["archive_digest"]
        checkpoint_archive(node, archive, data, max_bytes=LIMIT)
        install_and_restore(node, tag, image, engine, archive, diff_ids)
        if previous_binaries is not None:
            upgraded = verify_authority(node)
            assert upgraded["node_id"] == old["node_id"] and upgraded["database_format"] >= 132
            assert not upgraded["migration_required"], upgraded
            refused = subprocess.run([*previous, "serve"], capture_output=True, text=True, timeout=15)
            assert refused.returncode != 0 and "SchemaVersionUnsupported" in refused.stderr, refused
        print(f"PASS real {size}-byte OCI archive, independent quotas, durable disposal charge, detached checkpoint and Docker restore")
    finally:
        node.close()
        for reference in (tag, image):
            if reference:
                subprocess.run(DOCKER + ["image", "rm", reference], capture_output=True, timeout=15)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default=Path("target/debug"), type=Path)
    parser.add_argument("--previous-bin-dir", type=Path)
    args = parser.parse_args()
    main(args.bin_dir, args.previous_bin_dir)
