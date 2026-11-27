"""Read real Linux extended attributes from an OCI mount before and after rebind."""
import argparse
from io import BytesIO
import json
import os
from pathlib import Path
import signal
import subprocess
import tarfile

from oci_direct_clone_rebind import CLONE, REBIND, expect_bytes, grant, refuse_task, result
from support import Node, key, target


DOCKER = ["docker", "--host", "unix:///var/run/docker.sock"]
ATTRIBUTE = "user.psios-rebind"
ROOT_VALUE = b"volume root"
FILE_VALUE = b"bound file"
REPORT = b"read both Linux attributes\n"


def build_image(node, tag):
    source = node.base / "probe.c"
    source.write_text(r'''
#include <stdio.h>
#include <string.h>
#include <sys/xattr.h>
#include <unistd.h>

static int matches(const char *path, const char *expected) {
    char bytes[64];
    ssize_t count = getxattr(path, "user.psios-rebind", bytes, sizeof(bytes));
    return count == (ssize_t)strlen(expected) && !memcmp(bytes, expected, count);
}

int main(void) {
    if (!matches("/data", "volume root") || !matches("/data/starts", "bound file")) return 6;
    FILE *report = fopen("/data/starts", "a");
    if (!report || fputs("read both Linux attributes\n", report) < 0 || fclose(report) != 0) return 7;
    for (;;) pause();
}
''')
    probe = node.base / "probe"
    subprocess.run(["cc", "-O2", "-static", str(source), "-o", str(probe)], check=True, timeout=15)
    buffer = BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        archive.add(probe, arcname="probe")
    return subprocess.check_output(
        [*DOCKER, "import", "-", tag], input=buffer.getvalue(), timeout=30,
    ).decode().strip()


def main(binaries):
    node = Node(binaries)
    app_id = None
    image = None
    tag = "psios-xattr-integration:" + key()
    try:
        node.stop()
        settings = json.loads(node.config.read_text())
        node.config.write_text(json.dumps(dict(settings, backup_user_xattrs=True)))
        node.start()
        image = build_image(node, tag)
        volume = node.perform("volume.create", {"name": "original-with-xattrs"})
        root = node.state / "volumes" / volume["resource_id"] / "data"
        (root / "starts").touch()
        os.setxattr(root, ATTRIBUTE, ROOT_VALUE)
        os.setxattr(root / "starts", ATTRIBUTE, FILE_VALUE)
        app = node.perform("application.install", {
            "name": "user-xattr-rebind", "image_id": image, "argv": ["/probe"],
            "release_version": "1.0.0", "license_expression": "0BSD", "volume": target(volume),
            "cpu_millicores": 200, "memory_max_bytes": 33554432, "pids_max": 16,
            "ephemeral_max_bytes": 1048576,
        })
        app_id = app["resource_id"]
        started = node.perform("application.start", target(app))
        expect_bytes(root / "starts", REPORT)
        app = node.perform("application.stop", target(started))
        volume = node.get(volume["resource_id"])["reference"]
        prepared = node.prepare(
            CLONE, {"target": target(volume), "name": "cloned-with-xattrs"},
            delegation=grant(node, CLONE, [volume["resource_id"], "local"]),
        )
        submitted = node.call("Task.Submit", prepared)["task_id"]
        node.wait(submitted, autoapprove=True)
        clone = result(node, submitted)
        replacement = node.state / "volumes" / clone["resource_id"] / "data"
        assert replacement.stat().st_ino != root.stat().st_ino
        active = node.perform("volume.activate", target(clone))
        node.stop(signal.SIGKILL)
        node.config.write_text(json.dumps(settings))
        node.start()

        scope = grant(node, REBIND, [app_id, volume["resource_id"], clone["resource_id"]])
        inputs = {"target": target(app), "source_volume_ref": volume, "restored_volume_ref": active}
        original = node.get(app_id)
        os.setxattr(replacement, ATTRIBUTE, b"unexpected change")
        refuse_task(node, node.prepare(REBIND, inputs, delegation=scope), "PreconditionFailed")
        assert node.get(app_id) == original
        os.setxattr(replacement, ATTRIBUTE, ROOT_VALUE)
        accepted = node.call("Task.Submit", node.prepare(REBIND, inputs, delegation=scope))["task_id"]
        node.wait(accepted, autoapprove=True)
        receipt = result(node, accepted)
        assert receipt["restored_volume_ref"] == active

        started = node.perform("application.start", target(receipt["application_ref"]))
        expect_bytes(replacement / "starts", REPORT * 2)
        assert (root / "starts").read_bytes() == REPORT
        health = node.perform("application.health", target(started))
        assert health["process_running"] is True, health
        running, = json.loads(subprocess.check_output([*DOCKER, "inspect", health["container_id"]], timeout=15))
        mount, = [entry for entry in running["Mounts"] if entry["Destination"] == "/data"]
        assert mount["Source"] == str(replacement) and mount["RW"], mount
        stopped = node.perform("application.stop", target(started))
        node.perform("application.uninstall", target(stopped))
        print("PASS OCI process read the same user attributes before and after rebind and Core refused attribute drift")
    finally:
        node.close()
        if app_id is not None:
            containers = subprocess.check_output(
                [*DOCKER, "ps", "-aq", "--filter", f"label=io.psios.application={app_id}"], timeout=10,
            )
            for container in containers.decode().split():
                subprocess.run([*DOCKER, "rm", "-f", container], check=True, timeout=12, capture_output=True)
        if image is not None:
            subprocess.run([*DOCKER, "image", "rm", tag], check=True, timeout=12, capture_output=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    main(parser.parse_args().bin_dir)
