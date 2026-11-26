"""Observe the live mount of a genuine format-101 OCI instance after upgrading."""

import argparse
import hashlib
import json
from pathlib import Path
import signal
import subprocess
import tarfile

from oci_mount_observation import CONFIG, inspection, verify_kernel
from support import Node, key, target


def fingerprint(node):
    files = [*node.state.glob("state.db*"), *node.witness.iterdir()]
    result = {}
    for path in files:
        if path.is_file():
            info = path.stat()
            with path.open("rb") as file:
                digest = hashlib.file_digest(file, "sha256").hexdigest()
            result[path] = (info.st_ino, info.st_size, info.st_mtime_ns, digest)
    return result


def verify(node, expected_format, migration):
    report = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=25))
    assert report["migration_required"] == migration, report
    if migration:
        assert report["database_format"] == expected_format, report
    else:
        assert report["database_format"] >= expected_format, report
    assert (report["sqlite_integrity"], report["foreign_keys"], report["witness"]) == (
        "Verified", "Verified", "Confirmed"), report
    return report


def exercise(previous, current, probe):
    node = Node(previous)
    docker = ["docker", "--host", "unix:///var/run/docker.sock"]
    tag = "psios-effective-mount-upgrade:" + key()
    imported = False
    app_id = None
    try:
        rootfs = node.base / "probe-rootfs.tar"
        with tarfile.open(rootfs, "w") as archive:
            archive.add(probe.resolve(strict=True), arcname="busybox", recursive=False)
        image = subprocess.check_output([*docker, "import", rootfs, tag], text=True, timeout=25).strip()
        imported = True
        source = node.perform("volume.create", {"name": "legacy-mount-evidence"})
        content = "original verified migration data\n"
        written = node.perform("volume.write", {**target(source), "name": "evidence", "content": content})
        source = written["volume_ref"]
        data = node.state / "volumes" / source["resource_id"] / "data"
        installed = node.perform("application.install", {
            "name": "legacy-mount-evidence", "image_id": image, "volume": target(source),
            "release_version": "1.0.0", "argv": ["/busybox", "sleep", "180"], "license_expression": "0BSD",
            "cpu_millicores": 100, "memory_max_bytes": 32 * 1024 * 1024, "pids_max": 8,
            "ephemeral_max_bytes": 1024 * 1024,
        })
        app_id = installed["resource_id"]
        request = node.prepare("application.start", target(installed))
        accepted = node.call("Task.Submit", request)
        task = node.wait(accepted["task_id"], autoapprove=True)
        assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
        output, = task["result"]["artifact_refs"]
        app = node.call("Artifact.Read", {"reference": output})
        running = node.get(app_id)
        instance = node.get(running["value"]["instance_ref"]["resource_id"])
        volume = node.get(source["resource_id"])
        assert running["reference"] == app and running["value"]["lifecycle"] == "Running", running
        assert "mount_observation" not in running["value"]["last_observation"], running
        attachment, = volume["value"]["attachment_refs"]
        checkpoint_id = "oci-start-" + running["value"]["start_operation_id"]
        checkpoint = node.get(checkpoint_id)
        assert checkpoint["resource"]["kind"] == "OCIStartCheckpoint", checkpoint
        assert checkpoint["value"]["volume_ref"] == source, checkpoint
        assert "mount_observation_required" not in checkpoint["value"], checkpoint
        cid = running["value"]["container_id"]
        initial = inspection(docker, cid)
        assert initial["State"]["Running"] and initial["State"]["Pid"] > 0, initial["State"]
        pid = initial["State"]["Pid"]
        started_at = initial["State"]["StartedAt"]
        authority = node.call("Principal.WhoAmI", {})["authority_id"]

        old_daemon = node.daemon[0]
        old_config = node.config.read_bytes()
        node.stop(signal.SIGKILL)
        frozen = fingerprint(node)
        node.daemon[0] = str((current / "psiosd").resolve())
        before = verify(node, 101, True)
        assert before["node_id"] == authority and fingerprint(node) == frozen, before
        node.config.write_text(json.dumps({**json.loads(old_config), CONFIG: True}))
        node.start()
        assert node.get(checkpoint_id) == checkpoint
        assert node.get(app_id) == running and node.get(volume["reference"]["resource_id"]) == volume
        assert node.get(instance["reference"]["resource_id"]) == instance
        assert node.call("Task.Get", {"id": accepted["task_id"]}) == task
        assert node.call("Task.Submit", request) == {**accepted, "deduplicated": True}

        scope = node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", "application.inspect_instance"],
            "units": 2, "ttl_seconds": 90, "tool_targets": [app_id, volume["reference"]["resource_id"]],
        })
        prepared = node.prepare("application.inspect_instance", target(app),
                                delegation=scope["delegation_ref"]["resource_id"])
        observed = node.call("Task.Submit", prepared)
        observation_task = node.wait(observed["task_id"])
        assert (observation_task["lifecycle"], observation_task["outcome_knowledge"]) == (
            "Succeeded", "Known"), observation_task
        sample, = observation_task["result"]["artifact_refs"]
        report = node.call("Artifact.Read", {"reference": sample})
        proof = verify_kernel(docker, report, data, volume["reference"])
        assert (proof["container_id"], proof["host_pid"], proof["started_at"]) == (cid, pid, started_at), proof
        assert (Path(f"/proc/{pid}/root/data") / "evidence").read_text() == content
        assert node.get(checkpoint_id) == checkpoint and node.get(app_id) == running
        assert node.get(volume["reference"]["resource_id"])["value"]["attachment_refs"] == [attachment]

        app = node.perform("application.stop", target(app))
        assert not subprocess.check_output([*docker, "ps", "-aq", "--filter", "id=" + cid], timeout=10).strip()
        node.perform("application.uninstall", target(app))
        assert (data / "evidence").read_text() == content
        node.stop()
        after = verify(node, 102, False)
        assert after["node_id"] == authority, after
        frozen = fingerprint(node)
        # A format-101 daemon does not know the new config field. Restore the
        # original config to test its database-version fence directly.
        node.config.write_bytes(old_config)
        refused = subprocess.run([old_daemon, "--config", node.config, "check"],
                                 capture_output=True, text=True, timeout=15)
        assert refused.returncode != 0 and "SchemaVersionUnsupported" in refused.stderr, refused
        assert fingerprint(node) == frozen
        print("PASS live format-101 OCI attachment observed in format 102 with unchanged legacy proof and old-writer fence")
    finally:
        try:
            if app_id is not None:
                owned = subprocess.check_output([*docker, "ps", "-aq", "--no-trunc", "--filter",
                    "label=io.psios.application=" + app_id], text=True, timeout=10).split()
                for cid in owned:
                    subprocess.run([*docker, "rm", "--force", cid], check=True,
                                   stdout=subprocess.DEVNULL, timeout=10)
        finally:
            try:
                if imported:
                    subprocess.run([*docker, "image", "rm", tag], check=True, stdout=subprocess.DEVNULL, timeout=10)
            finally:
                node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", type=Path, required=True)
    parser.add_argument("--current-bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--probe", type=Path, default=Path("/usr/bin/busybox"), help="statically linked Linux test process")
    args = parser.parse_args()
    exercise(args.previous_bin_dir, args.current_bin_dir, args.probe)
