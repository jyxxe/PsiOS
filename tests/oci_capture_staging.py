"""Hold real Docker save receipts while testing OCI capture staging admission."""
import argparse
import hashlib
import json
from pathlib import Path
from queue import Queue
import subprocess
import threading
from urllib.parse import unquote_to_bytes

from oci_capture import CHUNK, DOCKER, MAX, SOCKET, TOOL, grant, import_fixture, no_lost_publication
from oci_recovery import ReceiptBarrier
from support import Node, key, target


LIMIT = 2 * MAX
STAGING_BUDGET = 2 * LIMIT


class SaveBarrier(ReceiptBarrier):
    def __init__(self, path):
        self.saves = Queue()
        super().__init__(path, SOCKET,
                         lambda request: b"GET " in request and b"/images/" in request and b"/get" in request,
                         status=b" 200 ", forward=True)

    def confirm(self, request):
        self.saves.put_nowait(unquote_to_bytes(request))
        return super().confirm(request)


def capture_while_busy(node, barrier, images, engine):
    grant(node, "local-owner", "artifact.upload")
    permit = grant(node, "local-owner", "task.submit", "task.read", TOOL)
    delegation = permit["delegation_ref"]["resource_id"]

    def approve(image):
        request = node.prepare(TOOL, {"image_id": image, "expected_engine_id": engine,
                                     "max_archive_bytes": LIMIT}, delegation=delegation)
        task = node.call("Task.Submit", request)["task_id"]
        return task, node.approve(task)["decision_id"]

    def paused_in_save(task, image):
        save = barrier.saves.get(timeout=12)
        assert image.encode() in save, save
        active = node.call("Task.Get", {"id": task})
        assert (active["lifecycle"], active["outcome_knowledge"]) == ("Running", "InFlight"), active
        return active

    def staging(amount):
        view = node.call("Node.Get", {})["oci_capture_staging"]
        assert view == {"max_bytes": STAGING_BUDGET, "reserved_bytes": amount}, view

    first, _ = approve(images[0])
    paused_in_save(first, images[0])
    second, _ = approve(images[1])
    paused_in_save(second, images[1])
    staging(STAGING_BUDGET)

    waiting, decision = approve(images[2])
    # This task was submitted after the third capture. Its receipt proves a
    # worker considered the ready queue while both Docker responses were held.
    control = node.perform("system.status", {})
    assert control["authority_id"] == node.call("Node.Get", {})["reference"]["resource_id"], control
    staging(STAGING_BUDGET)
    queued = node.call("Task.Get", {"id": waiting})
    assert (queued["lifecycle"], queued["outcome_knowledge"]) == ("Queued", "NotStarted"), queued
    history = node.call("Task.History", {"id": waiting})["items"]
    assert history and all(item["run_id"] is None for item in history), history
    use_id = f"use-{decision}"
    use = node.get(use_id)["value"]
    assert (use["binding_status"], use["validity_status"]) == ("Unbound", "Valid"), use
    assert barrier.saves.empty(), "an over-budget capture reached Docker"

    node.call("Task.Cancel", target(node.get(first)["reference"]))
    cancelled = node.wait(first)
    assert (cancelled["lifecycle"], cancelled["outcome_knowledge"]) == ("Cancelled", "Known"), cancelled
    assert cancelled["result"]["artifact_refs"] == [], cancelled
    resumed = paused_in_save(waiting, images[2])
    use = node.get(use_id)["value"]
    operation = resumed["operation_refs"][0]["resource_id"]
    assert use["binding_status"] == "Bound" and use["operation_id"] == operation, use
    assert operation == queued["operation_refs"][0]["resource_id"], resumed
    staging(STAGING_BUDGET)

    barrier.release.set()
    depot = node.state / "oci-archives"
    archive_ids = set()
    for task, image in ((second, images[1]), (waiting, images[2])):
        result = node.wait(task)
        assert (result["lifecycle"], result["outcome_knowledge"]) == ("Succeeded", "Known"), result
        receipt = node.call("Artifact.Read", {"reference": result["result"]["artifact_refs"][0]})
        reference = receipt["artifact_ref"]
        proof = node.call("Resource.Resolve", {"reference": receipt["capture_ref"]})["value"]
        archive_id = reference["resource_id"]
        archive_ids.add(archive_id)
        with (depot / archive_id / "payload").open("rb") as payload:
            digest = "sha256:" + hashlib.file_digest(payload, "sha256").hexdigest()
        assert reference["digest"] == digest == proof["archive_digest"], proof
        assert proof["image"]["config_digest"] == image, proof
    staging(0)
    no_lost_publication(node, 2)
    assert {item.name for item in depot.iterdir()} == archive_ids


def main(binaries):
    node = Node(binaries)
    name = "psios-staging-budget:" + key()
    tags = [f"{name}-{i}" for i in range(3)]
    try:
        images = [import_fixture(tag, CHUNK + i) for i, tag in enumerate(tags)]
        assert len(set(images)) == len(tags), images
        engine = subprocess.check_output(DOCKER + ["info", "--format", "{{.ID}}"], text=True, timeout=15).strip()
        socket = node.base / "held-save.sock"
        with SaveBarrier(socket) as barrier:
            server = threading.Thread(target=barrier.serve_forever, daemon=True)
            server.start()
            try:
                config = json.loads(node.config.read_text())
                node.stop()
                node.config.write_text(json.dumps(dict(config, docker_socket=str(socket),
                                                       oci_capture_max_bytes=LIMIT, workers=3)))
                node.start()
                capture_while_busy(node, barrier, images, engine)
            finally:
                barrier.release.set()
                barrier.shutdown()
                server.join(3)
        print("PASS real Docker staging waits without a new run or approval, admits other work, and reuses capacity")
    finally:
        try:
            node.close()
        finally:
            for tag in tags:
                subprocess.run(DOCKER + ["image", "rm", tag], capture_output=True, timeout=15)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default=Path("target/debug"), type=Path)
    main(parser.parse_args().bin_dir)
