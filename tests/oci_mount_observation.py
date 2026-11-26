"""Exercise effective OCI /data observation against a real Docker bind mount."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import re
import selectors
import signal
import socket
import socketserver
import sqlite3
import subprocess
import tarfile
import threading

from support import Node, key, target


CONFIG = "oci_effective_volume_observation"
START = re.compile(rb"(?:^|\r\n)POST (?:/v[0-9.]+)?/containers/([0-9a-f]{64})/start[? ]")


class BeforeStartRelay(socketserver.BaseRequestHandler):
    def handle(self):
        server = self.server
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream, selectors.DefaultSelector() as ready:
            upstream.connect(server.upstream)
            ready.register(self.request, selectors.EVENT_READ, upstream)
            ready.register(upstream, selectors.EVENT_READ, self.request)
            tail = b""
            try:
                while True:
                    for source, _ in ready.select():
                        data = source.fileobj.recv(65536)
                        if not data:
                            return
                        if source.fileobj is self.request and not server.arrived.is_set():
                            tail = (tail + data)[-8192:]
                            match = START.search(tail)
                            if match is not None:
                                server.container_id = match[1].decode("ascii")
                                server.arrived.set()
                                if not server.release.wait(15):
                                    return
                        source.data.sendall(data)
            except (BrokenPipeError, ConnectionResetError):
                return


class StartBarrier(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path, upstream):
        self.upstream = upstream
        self.arrived = threading.Event()
        self.release = threading.Event()
        self.container_id = None
        super().__init__(str(path), BeforeStartRelay)


def reconfigure(node, config, *, crash=False):
    node.stop(signal.SIGKILL if crash else signal.SIGTERM)
    node.config.write_text(json.dumps(config))
    node.start()


def inspection(docker, container):
    return json.loads(subprocess.check_output([*docker, "inspect", container], timeout=10))[0]


def verify_kernel(docker, report, data, reference):
    evidence = report["mount_observation"]
    assert evidence["profile"] == "LinuxLocalOciVolumeV1", evidence
    assert evidence["effective_access"] == "ReadWrite" and evidence["volume_ref"] == reference, evidence
    live = inspection(docker, evidence["container_id"])
    pid = live["State"]["Pid"]
    assert live["State"]["StartedAt"] == evidence["started_at"], live["State"]
    assert live["State"]["Running"] and pid == evidence["host_pid"], evidence
    root = Path(f"/proc/{pid}/root/data").stat()
    source = data.stat()
    namespace = Path(f"/proc/{pid}/ns/mnt").stat()
    assert os.path.samestat(root, source), (root, source)
    assert (root.st_dev, root.st_ino) == (int(evidence["device"]), int(evidence["inode"])), evidence
    assert (namespace.st_dev, namespace.st_ino) == (
        int(evidence["mount_namespace_device"]), int(evidence["mount_namespace_inode"])), evidence
    mounts = [line.split() for line in Path(f"/proc/{pid}/mountinfo").read_text().splitlines()]
    mount, = [fields for fields in mounts if fields[4] == "/data"]
    assert mount[0] == evidence["mount_id"] and mount[5].split(",")[0] == "rw", mount
    return evidence


def exercise(node, docker, specification, app_ids):
    original = json.loads(node.config.read_text())
    reconfigure(node, {**original, CONFIG: True})

    def install(name):
        volume = node.perform("volume.create", {"name": name})
        app = node.perform("application.install", {**specification, "name": name, "volume": target(volume)})
        app_ids.append(app["resource_id"])
        data = node.state / "volumes" / volume["resource_id"] / "data"
        return app, volume, data

    app, volume, data = install("effective-oci-mount")
    app = node.perform("application.start", target(app))
    running = node.get(app["resource_id"])["value"]
    start = verify_kernel(docker, running["last_observation"], data, volume)
    instance = node.get(running["instance_ref"]["resource_id"])["value"]
    assert instance["observation"]["mount_observation"] == start, instance
    volume = node.get(volume["resource_id"])["reference"]
    grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 5, "ttl_seconds": 120,
        "actions": ["task.submit", "task.read", "application.inspect_instance", "application.restart"],
        "tool_targets": [app["resource_id"], volume["resource_id"]]})
    delegated = grant["delegation_ref"]["resource_id"]

    def submit(tool, args):
        request = node.prepare(tool, args, delegation=delegated)
        receipt = node.call("Task.Submit", request)
        task = node.wait(receipt["task_id"], autoapprove=True)
        assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
        return node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})

    observed = submit("application.inspect_instance", target(app))
    live = verify_kernel(docker, observed, data, volume)
    app = submit("application.restart", {**target(app), "expected_boot_id": live["boot_id"],
                                            "expected_started_at": live["started_at"]})
    after = node.get(app["resource_id"])["value"]["last_observation"]
    refreshed = verify_kernel(docker, after, data, volume)
    assert refreshed["started_at"] != live["started_at"] and refreshed["container_id"] == live["container_id"], refreshed
    app = node.perform("application.stop", target(app))
    node.perform("application.uninstall", target(app))

    app, volume, data = install("recover-observed-oci-mount")
    app_id = app["resource_id"]
    database = node.state / "state.db"
    with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
        conn.execute(f"""CREATE TRIGGER interrupt_observed_oci_start BEFORE INSERT ON versions
            WHEN NEW.id='{app_id}' AND NEW.body->>'lifecycle'='Running'
            BEGIN SELECT RAISE(ABORT, 'injected effective mount receipt loss'); END""")
    try:
        started = node.submit("application.start", target(app))
        lost = node.wait(started, autoapprove=True)
        assert (lost["lifecycle"], lost["outcome_knowledge"]) == ("Paused", "Unknown"), lost
    finally:
        with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
            conn.execute("DROP TRIGGER interrupt_observed_oci_start")
    op = lost["operation_refs"][0]
    reconfigure(node, original, crash=True)
    sampled = node.wait(node.submit("application.health", target(app)))
    assert sampled["lifecycle"] == "Succeeded", sampled
    proof, = sampled["result"]["artifact_refs"]
    observed = node.call("Artifact.Read", {"reference": proof})
    evidence = verify_kernel(docker, observed, data, volume)
    node.call("Operation.Reconcile", {**target(op), "evidence_ref": proof})
    settled = node.wait(started)
    assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
    app = node.get(app_id)
    instance = node.get(app["value"]["instance_ref"]["resource_id"])["value"]
    assert instance["observation"]["mount_observation"] == evidence
    app = node.perform("application.stop", target(app["reference"]))
    node.perform("application.uninstall", target(app))
    reconfigure(node, {**original, CONFIG: True})

    app, volume, data = install("substituted-oci-mount")
    held = data.with_name("approved-before-start")
    wrong = data.with_name("substituted-held-mount")
    socket_path = node.base / "before-mount.sock"
    upstream = original.get("docker_socket", "/var/run/docker.sock")
    with StartBarrier(socket_path, upstream) as barrier:
        proxy = threading.Thread(target=barrier.serve_forever, daemon=True)
        proxy.start()
        try:
            reconfigure(node, {**original, CONFIG: True, "docker_socket": str(socket_path)})
            attempt = node.submit("application.start", target(app))
            assert node.wait(attempt)["lifecycle"] == "WaitingApproval"
            node.approve(attempt)
            assert barrier.arrived.wait(10), "did not intercept the original POST start"
            data.rename(held)
            data.mkdir(mode=0o700)
            approval_inode = held.stat()
            substituted = data.stat()
            assert not os.path.samestat(approval_inode, substituted)
            barrier.release.set()
            task = node.wait(attempt)
            assert (task["lifecycle"], task["outcome_knowledge"]) == ("Paused", "Unknown"), task
            cid = barrier.container_id
            actual = inspection(docker, cid)
            declaration, = actual["Mounts"]
            assert declaration["Source"] == str(data) and declaration["Destination"] == "/data", declaration
            mounted = Path(f"/proc/{actual['State']['Pid']}/root/data")
            assert os.path.samestat(mounted.stat(), substituted)
            assert node.get(app["resource_id"])["reference"] == app
            original_volume = node.get(volume["resource_id"])
            assert original_volume["reference"] == volume and original_volume["value"]["attachment_refs"] == []
            data.rename(wrong)
            held.rename(data)
            assert os.path.samestat(data.stat(), approval_inode) and os.path.samestat(mounted.stat(), substituted)

            # Turning the profile off after a witnessed start must not let a
            # recovery observation attest the wrong inode by pathname alone.
            reconfigure(node, original, crash=True)
            denied = node.wait(node.submit("application.health", target(app)))
            assert denied["lifecycle"] == "Failed" and not denied["result"]["artifact_refs"], denied
            assert node.call("Task.Get", {"id": attempt}) == task
            blocked = node.wait(node.submit("volume.read", {**target(volume), "name": "held"}))
            assert blocked["lifecycle"] == "Failed" and blocked["conditions"][0]["type"] == "Conflict", blocked
            assert node.get(volume["resource_id"]) == original_volume
        finally:
            barrier.release.set()
            barrier.shutdown()
            proxy.join(5)
            if held.exists():
                if data.exists():
                    data.rename(wrong)
                held.rename(data)
    node.stop()
    subprocess.run([*node.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)
    print("PASS real effective OCI mount, restart, wrong inode rejection, and durable recovery fence")


def main(binaries, busybox):
    node = Node(binaries)
    docker = ["docker", "--host", "unix:///var/run/docker.sock"]
    tag = "psios-effective-mount:" + key()
    app_ids = []
    imported = False
    try:
        raw = node.base / "probe-rootfs.tar"
        with tarfile.open(raw, "w") as archive:
            archive.add(busybox.resolve(strict=True), arcname="busybox", recursive=False)
        image = subprocess.check_output([*docker, "import", raw, tag], text=True, timeout=25).strip()
        imported = True
        specification = {"image_id": image, "release_version": "1.0.0", "argv": ["/busybox", "sleep", "180"],
                         "license_expression": "0BSD", "cpu_millicores": 100, "memory_max_bytes": 32 * 1024 * 1024,
                         "pids_max": 8, "ephemeral_max_bytes": 1024 * 1024}
        exercise(node, docker, specification, app_ids)
    finally:
        for application in app_ids:
            ids = subprocess.check_output([*docker, "ps", "-aq", "--filter", "label=io.psios.application=" + application],
                                          text=True, timeout=10).split()
            for container in ids:
                subprocess.run([*docker, "rm", "--force", container], check=True, stdout=subprocess.DEVNULL, timeout=10)
        if imported:
            subprocess.run([*docker, "image", "rm", tag], check=True, stdout=subprocess.DEVNULL, timeout=10)
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/debug")
    parser.add_argument("--busybox", type=Path, default=Path("/usr/bin/busybox"), help="statically linked local Linux BusyBox")
    args = parser.parse_args()
    main(args.bin_dir, args.busybox)
