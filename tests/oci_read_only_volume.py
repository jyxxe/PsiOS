"""Exercise an approved read-only data volume in an actual local Docker container."""
import argparse
from contextlib import closing
from io import BytesIO
import json
from pathlib import Path
import signal
import sqlite3
import subprocess
import tarfile
import time

from oci import stage_from_artifact
from oci_mount_observation import verify_kernel
from support import Failure, Node, key, target


DOCKER = ["docker", "--host", "unix:///var/run/docker.sock"]


def build_image(node, tag):
    source = node.base / "probe.c"
    source.write_text(r'''
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    char data[32] = {0};
    int seed = open("/data/seed", O_RDONLY);
    if (seed < 0 || read(seed, data, sizeof(data) - 1) != 8 || strcmp(data, "original")) return 4;
    close(seed);
    if (argc == 2 && strcmp(argv[1], "write-capable") == 0) {
        int out = open("/data/old-writer", O_WRONLY | O_CREAT | O_APPEND, 0600);
        if (out < 0 || write(out, "legacy write\n", 13) != 13 || close(out) < 0) return 7;
        for (;;) pause();
    }
    if (open("/data/seed", O_WRONLY | O_APPEND) >= 0 || errno != EROFS) return 5;
    if (open("/data/new", O_WRONLY | O_CREAT, 0600) >= 0 || errno != EROFS) return 6;
    puts("read original and both writes returned EROFS");
    fflush(stdout);
    for (;;) pause();
}
''')
    probe = node.base / "probe"
    subprocess.run(["cc", "-O2", "-static", str(source), "-o", str(probe)], check=True, timeout=15)
    buffer = BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        archive.add(probe, arcname="probe")
    return subprocess.check_output([*DOCKER, "import", "-", tag], input=buffer.getvalue(), timeout=30).decode().strip()


def release_mode(node, release):
    manifest = node.call("Resource.Resolve", {"reference": release})["value"]
    specification = node.call("Resource.Resolve", {"reference": manifest["release_spec_ref"]})["value"]
    binding, = specification["permission_requests"]["volume_bindings"]
    assert (binding["manifest_volume_name"], binding["mount_path"]) == ("data", "/data"), binding
    return binding["requested_access"]


def verify_application(node, app, path, volume):
    state = node.get(app["resource_id"])["value"]
    evidence = verify_kernel(DOCKER, state["last_observation"], path, volume, access="ReadOnly")
    cutoff = time.monotonic() + 8
    while True:
        log = subprocess.check_output([*DOCKER, "logs", evidence["container_id"]], timeout=5)
        if b"read original and both writes returned EROFS" in log:
            break
        assert time.monotonic() < cutoff, log
        time.sleep(0.03)
    current = node.get(volume["resource_id"])["value"]
    reference, = current["attachment_refs"]
    attachment = node.call("Resource.Resolve", {"reference": reference})["value"]
    assert (attachment["requested_access"], attachment["effective_access"], attachment["lifecycle"]) == (
        "ReadOnly", "ReadOnly", "Attached"), attachment
    return evidence, reference


def application_spec(image, name):
    return {"name": name, "image_id": image, "argv": ["/probe"], "release_version": "1.0.0",
            "license_expression": "0BSD", "cpu_millicores": 100, "memory_max_bytes": 33554432,
            "pids_max": 8, "ephemeral_max_bytes": 1048576}


def seed_volume(node, name):
    volume = node.perform("volume.create", {"name": name})
    return node.perform("volume.write", {**target(volume), "name": "seed", "content": "original"})["volume_ref"]


def upgrade_running_read_write_volume(node, image, binaries, app_ids):
    config = json.loads(node.config.read_text())
    node.stop()
    node.config.write_text(json.dumps(dict(config, oci_effective_volume_observation=True)))
    node.start()
    volume = seed_volume(node, "legacy-writable-source")
    data = node.state / "volumes" / volume["resource_id"] / "data"
    spec = application_spec(image, "legacy-writable-app")
    app = node.perform("application.install", dict(spec, volume=target(volume), argv=["/probe", "write-capable"]))
    app_id = app["resource_id"]
    app_ids.append(app_id)
    app = node.perform("application.start", target(app))
    old_app = node.get(app_id)
    old_volume = node.get(volume["resource_id"])
    observed = old_app["value"]["last_observation"]
    old_mount = verify_kernel(DOCKER, observed, data, volume)
    cutoff = time.monotonic() + 5
    written = data / "old-writer"
    while not written.is_file() or written.read_bytes() != b"legacy write\n":
        assert time.monotonic() < cutoff
        time.sleep(0.03)

    node.stop(signal.SIGKILL)
    old = node.daemon.copy()
    node.binaries = binaries.absolute()
    node.daemon[0] = str(node.binaries / "psiosd")
    pending = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert (pending["database_format"], pending["migration_required"]) == (139, True), pending
    node.start()
    assert node.get(app_id) == old_app and node.get(volume["resource_id"]) == old_volume
    assert verify_kernel(DOCKER, observed, data, volume) == old_mount
    stopped = node.perform("application.stop", target(app))
    node.perform("application.uninstall", target(stopped))
    assert (data / "old-writer").read_bytes() == b"legacy write\n"
    node.stop()
    witness = (node.witness / "head").read_bytes()
    fenced = subprocess.run([*old, "serve"], capture_output=True, timeout=15)
    assert fenced.returncode and b"SchemaVersionUnsupported" in fenced.stderr, fenced
    assert (node.witness / "head").read_bytes() == witness
    node.config.write_text(json.dumps(config))
    node.start()
    print("PASS format 139 kept its live writable mount and data through migration, and the old writer was fenced")


def recover_unsettled_start(node, app, data, volume):
    app_id = app["resource_id"]
    database = node.state / "state.db"
    with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
        conn.execute(f"""CREATE TRIGGER interrupt_read_only_start BEFORE INSERT ON versions
            WHEN NEW.id='{app_id}' AND NEW.body->>'lifecycle'='Running'
            BEGIN SELECT RAISE(ABORT, 'injected read-only start receipt loss'); END""")
    try:
        started = node.submit("application.start", target(app))
        lost = node.wait(started, autoapprove=True)
        assert (lost["lifecycle"], lost["outcome_knowledge"]) == ("Paused", "Unknown"), lost
    finally:
        with closing(sqlite3.connect(database, timeout=10)) as conn, conn:
            conn.execute("DROP TRIGGER interrupt_read_only_start")
    container, = subprocess.check_output([*DOCKER, "ps", "-q", "--no-trunc", "--filter",
        f"label=io.psios.application={app_id}"], timeout=10).decode().split()
    node.stop(signal.SIGKILL)
    node.start()
    observed = node.wait(node.submit("application.health", target(app)))
    assert (observed["lifecycle"], observed["outcome_knowledge"]) == ("Succeeded", "Known"), observed
    proof, = observed["result"]["artifact_refs"]
    report = node.call("Artifact.Read", {"reference": proof})
    evidence = verify_kernel(DOCKER, report, data, volume, access="ReadOnly")
    assert evidence["container_id"] == container
    operation, = lost["operation_refs"]
    node.call("Operation.Reconcile", {**target(operation), "evidence_ref": proof})
    settled = node.wait(started)
    assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
    app = node.get(app_id)["reference"]
    actual, _ = verify_application(node, app, data, volume)
    assert actual == evidence
    print("PASS recovered the original read-only start after an actual lost SQLite receipt")
    return app


def exercise(node, image, app_ids):
    config = json.loads(node.config.read_text())
    assert not config.get("oci_effective_volume_observation"), config
    spec = application_spec(image, "read-only-oci-volume")
    try:
        node.submit("application.install", dict(spec, volume_access="ReadOnly"))
    except Failure as failure:
        assert failure.code == "InvalidArgument", failure
    else:
        raise AssertionError("read-only launch must name a managed volume")

    volume = seed_volume(node, "protected-source")
    data = node.state / "volumes" / volume["resource_id"] / "data"
    app = node.perform("application.install", dict(spec, volume=target(volume), volume_access="ReadOnly"))
    app_id = app["resource_id"]
    app_ids.append(app_id)
    assert release_mode(node, node.get(app_id)["resource"]["spec"]["release_ref"]) == "ReadOnly"
    app = node.perform("application.start", target(app))
    evidence, attachment = verify_application(node, app, data, volume)
    volume = node.get(volume["resource_id"])["reference"]
    refused = node.wait(node.submit("volume.write", {**target(volume), "name": "new", "content": "denied"}), autoapprove=True)
    assert (refused["lifecycle"], refused["conditions"][0]["type"]) == ("Failed", "Conflict"), refused

    node.stop(signal.SIGKILL)
    offline = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert offline["database_format"] >= 140 and offline["sqlite_integrity"] == "Verified", offline
    node.start()
    grant = node.call("Grant.Issue", {"principal_id": "local-owner", "actions": ["task.submit", "task.read",
        "application.inspect_instance", "application.restart"], "tool_targets": [app_id, volume["resource_id"]],
        "units": 4, "ttl_seconds": 120})

    def delegated(tool, query):
        request = node.prepare(tool, query, delegation=grant["delegation_ref"]["resource_id"])
        settled = node.wait(node.call("Task.Submit", request)["task_id"], autoapprove=True)
        assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Succeeded", "Known"), settled
        result, = settled["result"]["artifact_refs"]
        return node.call("Artifact.Read", {"reference": result})

    observed = delegated("application.inspect_instance", target(app))
    assert verify_kernel(DOCKER, observed, data, volume, access="ReadOnly")["mount_id"] == evidence["mount_id"]
    app = delegated("application.restart", {**target(app), "expected_boot_id": observed["boot_id"],
                                             "expected_started_at": observed["started_at"]})
    after, same_attachment = verify_application(node, app, data, volume)
    assert after["container_id"] == evidence["container_id"] and after["started_at"] != evidence["started_at"]
    assert same_attachment == attachment
    app = node.perform("application.stop", target(app))
    detached = node.get(attachment["resource_id"])["value"]
    assert (detached["effective_access"], detached["lifecycle"]) == ("Blocked", "Detached"), detached

    staged, _, _, _ = stage_from_artifact(node, app, dict(spec, release_version="2.0.0"))
    assert release_mode(node, staged) == "ReadOnly"
    app = node.perform("application.select_release", {"target": target(app), "release_ref": staged})
    volume = node.get(volume["resource_id"])["reference"]
    app = recover_unsettled_start(node, app, data, volume)
    app = node.perform("application.stop", target(app))
    node.perform("application.uninstall", target(app))
    assert (data / "seed").read_bytes() == b"original" and not (data / "new").exists()
    volume = node.get(volume["resource_id"])["reference"]
    node.perform("volume.write", {**target(volume), "name": "new", "content": "after detach"})
    assert (data / "new").read_bytes() == b"after detach"
    node.stop()
    final = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert (final["witness"], final["sqlite_integrity"]) == ("Confirmed", "Verified"), final
    print("PASS read-only Linux I/O, OCI restart, signed staged release, detach and offline authority verification")


def main(binary_dir, previous_dir):
    node = Node(previous_dir or binary_dir)
    tag = "psios-read-only-volume:" + key()
    image = None
    app_ids = []
    try:
        image = build_image(node, tag)
        if previous_dir is not None:
            upgrade_running_read_write_volume(node, image, binary_dir, app_ids)
        exercise(node, image, app_ids)
    finally:
        node.close()
        for app_id in app_ids:
            containers = subprocess.check_output([*DOCKER, "ps", "-aq", "--filter",
                f"label=io.psios.application={app_id}"], timeout=10).decode().split()
            for container in containers:
                subprocess.run([*DOCKER, "rm", "-f", container], check=True, capture_output=True, timeout=12)
        if image is not None:
            subprocess.run([*DOCKER, "image", "rm", tag], check=True, capture_output=True, timeout=12)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--previous-bin-dir", type=Path)
    args = parser.parse_args()
    main(args.bin_dir, args.previous_bin_dir)
