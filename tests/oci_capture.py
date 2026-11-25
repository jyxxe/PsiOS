"""Capture exact bytes from the real Docker Engine through a scoped, approved Task."""
import argparse
import base64
import hashlib
import io
import json
from pathlib import Path
import random
import signal
import sqlite3
import subprocess
import tarfile
import threading

from oci_recovery import ReceiptBarrier
from support import Failure, Node, key, target


TOOL = "application.capture_oci_image"
MEDIA = "application/vnd.psios.docker-image.v1+tar"
MAX = 8 * 1024 * 1024
CHUNK = 128 * 1024
SOCKET = "/var/run/docker.sock"
DOCKER = ["docker", "--host", "unix://" + SOCKET]


def grant(node, principal, *actions):
    return node.call("Grant.Issue", {"principal_id": principal, "actions": actions,
        "units": 32, "ttl_seconds": 300,
        "tool_targets": [node.call("Node.Get", {})["reference"]["resource_id"]]})


def denied(node, code, method, params, token=None):
    try:
        node.call(method, params, token)
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"{method} unexpectedly succeeded")


def import_fixture(tag, size):
    with io.BytesIO() as output:
        with tarfile.open(fileobj=output, mode="w") as archive:
            member = tarfile.TarInfo("capture-fixture")
            member.size = size
            archive.addfile(member, io.BytesIO(random.Random(size).randbytes(size)))
        return subprocess.check_output(DOCKER + ["import", "-", tag], input=output.getvalue(),
                                       timeout=30).decode().strip()


def assert_failed(settlement, code):
    assert (settlement["lifecycle"], settlement["outcome_knowledge"]) == ("Failed", "Known"), settlement
    assert settlement["conditions"][0]["type"] == code and not settlement["result"]["artifact_refs"], settlement


def no_lost_publication(node, expected):
    with sqlite3.connect(node.state / "state.db") as db:
        assert db.execute("SELECT count(*) FROM oci_image_captures").fetchone() == (expected,)
        assert db.execute("SELECT count(*) FROM artifact_uploads WHERE source_kind='LocalOciImageCapture'").fetchone() == (expected,)
        assert db.execute("SELECT count(*) FROM artifact_upload_chunks").fetchone() == (0,)


def interrupted(node, create_task, operation, revoke=None):
    original = json.loads(node.config.read_text())
    path = node.base / (operation + ".sock")
    with ReceiptBarrier(path, SOCKET, lambda req: b"GET " in req and b"/images/" in req and b"/get" in req,
                        status=b" 200 ") as barrier:
        serving = threading.Thread(target=barrier.serve_forever, daemon=True)
        serving.start()
        try:
            node.stop()
            node.config.write_text(json.dumps(dict(original, docker_socket=str(path))))
            node.start()
            task = create_task()
            node.approve(task)
            assert barrier.confirmed.wait(12), f"real Docker save did not start before {operation}"
            if operation == "cancel":
                node.call("Task.Cancel", target(node.get(task)["reference"]))
                state = node.wait(task)
                assert (state["lifecycle"], state["outcome_knowledge"]) == ("Cancelled", "Known"), state
                assert not state["result"]["artifact_refs"], state
            elif operation == "revoke":
                node.call("Grant.Revoke", target(revoke))
                assert_failed(node.wait(task), "PermissionDenied")
            else:
                node.stop(signal.SIGKILL)
        finally:
            barrier.release.set()
            barrier.shutdown()
            serving.join(3)
    if operation == "restart":
        node.config.write_text(json.dumps(original))
        node.start()
        assert_failed(node.wait(task), "LeaseLost")
    else:
        node.stop()
        node.config.write_text(json.dumps(original))
        node.start()


def main(binaries):
    n = Node(binaries)
    tag = "psios-image-capture:" + key()
    large_tag = tag + "-large"
    image_id = large_id = None
    try:
        image_id = import_fixture(tag, CHUNK + 8192)
        engine = subprocess.check_output(DOCKER + ["info", "--format", "{{.ID}}"], text=True, timeout=15).strip()
        request = {"image_id": image_id, "expected_engine_id": engine}
        owner = "local-owner"
        outside = n.call("Agent.Register", {"name": "untrusted-oci-capture"})
        node_id = n.call("Node.Get", {})["reference"]["resource_id"]
        denied(n, "PermissionDenied", "Grant.Issue", {"principal_id": outside["principal_id"],
            "actions": [TOOL], "ttl_seconds": 60, "units": 1, "tool_targets": [node_id]})
        scope = grant(n, owner, "task.submit", "task.read", TOOL)["delegation_ref"]["resource_id"]
        denied(n, "PermissionDenied", "Task.Submit", n.prepare(TOOL, request, delegation=scope))
        grant(n, owner, "artifact.upload")

        def submit(delegation=scope, **overrides):
            return n.call("Task.Submit", n.prepare(TOOL, dict(request, **overrides), delegation=delegation))["task_id"]

        denied(n, "InvalidArgument", "Task.Prepare", {"tool": TOOL, "input": dict(request, image_id=tag),
            "key": key(), "delegation_id": scope})
        assert_failed(n.wait(submit(expected_engine_id="unapproved-engine-id"), autoapprove=True), "PreconditionFailed")
        assert_failed(n.wait(submit(image_id="sha256:" + "0" * 64), autoapprove=True), "DependencyUnavailable")
        no_lost_publication(n, 0)

        prepared = n.prepare(TOOL, request, delegation=scope)
        accepted = n.call("Task.Submit", prepared)
        task = accepted["task_id"]
        approval = n.wait(task)
        assert approval["lifecycle"] == "WaitingApproval" and not approval["result"]["artifact_refs"], approval
        plan = n.get(approval["conditions"][0]["message"])["value"]["plan_ref"]
        purpose = n.call("Resource.Resolve", {"reference": plan})["value"]["purpose_ref"]
        assert n.call("Resource.Resolve", {"reference": purpose})["value"] == request
        no_lost_publication(n, 0)
        succeeded = n.wait(task, autoapprove=True)
        assert succeeded["lifecycle"] == "Succeeded", succeeded
        receipt = n.call("Artifact.Read", {"reference": succeeded["result"]["artifact_refs"][0]})
        raw = receipt["artifact_ref"]
        proof = n.call("Resource.Resolve", {"reference": receipt["capture_ref"]})["value"]
        assert proof["artifact_ref"] == raw and proof["archive_format"] == "DockerImageArchiveV1"
        assert proof["engine_id"] == engine and proof["image"]["config_digest"] == image_id
        compatible = {"status": "Verified", "profile": "DockerClassicTaglessSaveV1"}
        assert proof["restore_compatibility"] == receipt["restore_compatibility"] == compatible, proof
        denied(n, "PermissionDenied", "Artifact.ReadChunk", {"reference": raw, "offset": 0, "length": 1})
        remove = {"reference": raw, "reason": "completed standalone capture acceptance"}
        denied(n, "PermissionDenied", "Artifact.Withdraw", remove)
        grant(n, owner, "artifact.read")

        n.stop(signal.SIGKILL)
        n.start()
        replay = n.call("Task.Submit", prepared)
        assert replay["deduplicated"] and replay["task_id"] == task, replay
        parsed = n.call("Artifact.Get", {"reference": raw})
        assert parsed["resource"]["spec"]["declared_media_type"] == MEDIA
        assert parsed["resource"]["spec"]["source"]["kind"] == "LocalOciImageCapture"
        data = bytearray()
        while True:
            reply = n.call("Artifact.ReadChunk", {"reference": raw, "offset": len(data), "length": CHUNK})
            data.extend(base64.b64decode(reply["data_base64"], validate=True))
            if reply["next_offset"] is None:
                break
        assert CHUNK < len(data) == int(proof["size_bytes"]) <= MAX
        assert "sha256:" + hashlib.sha256(data).hexdigest() == proof["archive_digest"]
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tar:
            manifest, = json.load(tar.extractfile("manifest.json"))
            assert not manifest.get("RepoTags"), manifest
            config = tar.extractfile(manifest["Config"]).read()
            assert "sha256:" + hashlib.sha256(config).hexdigest() == image_id
            assert json.loads(config)["rootfs"]["diff_ids"] == proof["image"]["diff_ids"]

        subprocess.run(DOCKER + ["image", "rm", tag], check=True, capture_output=True, timeout=15)
        assert subprocess.run(DOCKER + ["image", "inspect", image_id], capture_output=True, timeout=15).returncode != 0
        # Exercise recovery bytes independently of the PsiOS client or any registry.
        subprocess.run(DOCKER + ["image", "load", "--quiet"], input=bytes(data), check=True,
                       capture_output=True, timeout=25)
        materialized, = json.loads(subprocess.check_output(DOCKER + ["image", "inspect", image_id], timeout=15))
        assert materialized["Id"] == image_id and not materialized["RepoTags"], materialized
        assert materialized["RootFS"]["Layers"] == proof["image"]["diff_ids"], materialized

        staged = [n.call("Artifact.Stage", {"key": key(), "size_bytes": MAX, "ttl_seconds": 60,
            "declared_media_type": "application/octet-stream", "digest": "sha256:" + "1" * 64}) for _ in range(3)]
        # Fill the remaining owner capacity with the fourth upload reservation.
        remaining = MAX - len(data)
        staged += [n.call("Artifact.Stage", {"key": key(), "size_bytes": remaining, "ttl_seconds": 60,
            "declared_media_type": "application/octet-stream", "digest": "sha256:" + "2" * 64})]
        assert_failed(n.wait(submit(), autoapprove=True), "QuotaExceeded")
        for stage in staged:
            n.call("Artifact.Stage.Discard", {"stage_id": stage["stage_id"]})

        large_id = import_fixture(large_tag, MAX + CHUNK)
        assert_failed(n.wait(submit(image_id=large_id), autoapprove=True), "QuotaExceeded")
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("CREATE TRIGGER reject_capture BEFORE INSERT ON oci_image_captures "
                       "BEGIN SELECT RAISE(ABORT,'injected capture settlement failure'); END")
        assert_failed(n.wait(submit(), autoapprove=True), "StorageUnavailable")
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("DROP TRIGGER reject_capture")
        no_lost_publication(n, 1)

        interrupted(n, submit, "cancel")
        interrupted(n, submit, "restart")
        revoked = grant(n, owner, "task.submit", "task.read", TOOL)
        interrupted(n, lambda: submit(delegation=revoked["delegation_ref"]["resource_id"]),
                    "revoke", revoked["grant_ref"])
        no_lost_publication(n, 1)

        grant(n, owner, "artifact.withdraw")
        withdrawal = n.call("Artifact.Withdraw", remove)
        assert n.call("Artifact.Withdraw", remove) == withdrawal
        denied(n, "ResultEvicted", "Artifact.ReadChunk", {"reference": raw, "offset": 0, "length": 1})
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=15)
        with sqlite3.connect(n.state / "state.db") as db:
            pin = db.execute("SELECT artifact,proof,restore_status FROM oci_image_captures").fetchone()
            db.execute("DELETE FROM oci_image_captures")
        corrupt = subprocess.run(n.daemon + ["verify"], capture_output=True, timeout=15)
        assert corrupt.returncode != 0 and b"OCI capture provenance index is inconsistent" in corrupt.stderr, corrupt
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("INSERT INTO oci_image_captures VALUES(?,?,?)", pin)
            db.execute("UPDATE oci_image_captures SET restore_status='Unverified'")
        downgraded = subprocess.run(n.daemon + ["verify"], capture_output=True, timeout=15)
        assert downgraded.returncode != 0 and b"OCI capture compatibility evidence disagrees" in downgraded.stderr, downgraded
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("UPDATE oci_image_captures SET restore_status=?", (pin[2],))
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=15)
        print("PASS approved Docker exact-ID capture, real reload, quotas, cancellation, revocation and restart")
    finally:
        n.close()
        for ref in (tag, large_tag, image_id, large_id):
            if ref:
                subprocess.run(DOCKER + ["image", "rm", ref], capture_output=True, timeout=15)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default=Path("target/debug"), type=Path)
    main(parser.parse_args().bin_dir)
