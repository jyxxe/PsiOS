"""Exercise an approved read-only data volume in an actual local Docker container."""
import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from io import BytesIO
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tarfile
import time

from oci import stage_from_artifact
from oci_mount_observation import verify_kernel
from support import Failure, Node, key, target
from test_mcp_stdio import Wire


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


def agent_volume(node):
    agent = node.call("Agent.Register", {"name": "attached-volume-agent", "peer_uid": os.getuid()})
    credential = node.call("Grant.Issue", {"principal_id": agent["principal_id"],
        "actions": ["task.submit", "task.read", "volume.create"], "tool_targets": ["local"],
        "units": 1, "ttl_seconds": 120})
    request = node.prepare("volume.create", {"name": "protected-source"}, token=agent["token"],
                           delegation=credential["delegation_ref"]["resource_id"])
    task = node.wait(node.call("Task.Submit", request, agent["token"])["task_id"], autoapprove=True)
    assert task["lifecycle"] == "Succeeded", task
    reference, = task["result"]["artifact_refs"]
    volume = node.call("Artifact.Read", {"reference": reference}, agent["token"])
    seeded = node.perform("volume.write", {**target(volume), "name": "seed", "content": "original"})
    return seeded["volume_ref"], agent


def verify_shared_reads(node, volume):
    current = node.get(volume["resource_id"])["reference"]
    files = {"target": target(current)}
    chunk = {**files, "path": "seed", "offset": 0, "limit": 8}
    with ThreadPoolExecutor(max_workers=2) as readers:
        page = readers.submit(node.perform, "volume.files", files)
        read = readers.submit(node.perform, "volume.read_chunk", chunk)
        page, read = page.result(), read.result()
    assert {"name": "seed", "kind": "file", "bytes": "8"} in page["entries"], page
    assert base64.b64decode(read["data_base64"], validate=True) == b"original", read
    assert page["volume_version"] == read["volume_version"] == current["version"]
    backup = node.wait(node.submit("backup.create", target(current)), autoapprove=True)
    assert (backup["lifecycle"], backup["conditions"][0]["type"]) == ("Failed", "Conflict"), backup


def verify_agent_read(node, volume, agent):
    volume_id = volume["resource_id"]
    tools = ["volume.files", "volume.read_chunk"]
    grant = node.call("Grant.Issue", {"principal_id": agent["principal_id"],
        "actions": ["task.submit", "task.read", *tools], "tool_targets": [volume_id],
        "units": 2, "ttl_seconds": 120})
    token = node.base / "read-only-agent.token"
    token.touch(mode=0o600)
    token.write_text(agent["token"])
    wire = Wire([str(node.binaries / "psios-mcp"), "--socket", str(node.socket),
                 "--token-file", str(token), "--delegation", grant["delegation_ref"]["resource_id"],
                 "--volume-files", volume_id, "--volume-read-chunk", volume_id])
    try:
        wire.initialize()
        listed = wire.request("tools/list")["result"]["tools"]
        assert [tool["name"] for tool in listed] == tools, listed
        listed_files = wire.invoke(tools[0])
        assert not listed_files["isError"], listed_files
        assert listed_files["structuredContent"]["result"]["entries"] == [
            {"name": "seed", "kind": "file", "bytes": "8"}], listed_files
        read = wire.request("tools/call", {"name": tools[1], "arguments": {"path": "seed"}})["result"]
        assert not read["isError"], read
        receipt = read["structuredContent"]
        assert base64.b64decode(receipt["result"]["data_base64"], validate=True) == b"original", read
        task = node.call("Task.Get", {"id": receipt["task_id"]}, agent["token"])
        artifact, = task["result"]["artifact_refs"]
        assert node.call("Artifact.Read", {"reference": artifact}, agent["token"]) == receipt["result"]
    finally:
        wire.close()
    print("PASS a scoped MCP Agent read the authorized volume while the OCI process remained active")


def verify_cgroup_usage(report):
    limits = report["effective_limits"]
    assert limits["source"] == "LinuxCgroupV2", limits
    assert report["process_id"] == limits["host_pid"], report
    usage = report["resource_usage"]
    assert int(usage["from_boot_us"]) <= int(usage["through_boot_us"]), usage
    measured = usage["cgroup"]
    assert measured["source"] == "LinuxCgroupV2" and measured["inode"] == limits["cgroup_inode"], measured
    membership = Path(f'/proc/{report["process_id"]}/cgroup').read_text()
    relative = membership.removeprefix("0::/").strip()
    group = Path("/sys/fs/cgroup") / relative
    assert str(group.stat().st_ino) == limits["cgroup_inode"], group
    cpu = dict(line.split() for line in (group / "cpu.stat").read_text().splitlines())
    stat = measured["cpu"]["stat"]
    assert stat["status"] == "Observed" and 0 <= stat["value"]["usage_usec"] <= int(cpu["usage_usec"]), stat
    memory = measured["memory"]["current_bytes"]
    assert memory["status"] == "Observed" and 0 < memory["value"] <= limits["memory_max_bytes"], memory
    pids = measured["pids"]["current"]
    assert pids["status"] == "Observed" and pids["value"] == int((group / "pids.current").read_text()), pids


def refuse_other_invocation(node, spec, volume, app_ids):
    app = node.perform("application.install", dict(spec, name="external-oci-restart",
                                                   volume=target(volume), volume_access="ReadOnly"))
    app_id = app["resource_id"]
    app_ids.append(app_id)
    app = node.perform("application.start", target(app))
    current = node.get(app_id)["value"]
    container = current["container_id"]
    before = current["last_observation"]["mount_observation"]["started_at"]
    subprocess.run([*DOCKER, "restart", "--timeout", "0", container], check=True, timeout=15,
                   stdout=subprocess.DEVNULL)
    state = json.loads(subprocess.check_output(
        [*DOCKER, "inspect", "--format", "{{json .State}}", container], timeout=10))
    assert state["Running"] and state["StartedAt"] != before, state
    assert node.get(app_id)["reference"] == app
    attached = node.get(volume["resource_id"])["reference"]
    task_id = node.submit("volume.read_chunk", {"target": target(attached),
                                               "path": "seed", "offset": 0, "limit": 8})
    result = node.wait(task_id)
    assert (result["lifecycle"], result["conditions"][0]["type"]) == ("Failed", "PreconditionFailed"), result
    assert result["result"]["artifact_refs"] == [], result
    with closing(sqlite3.connect(node.state / "state.db")) as conn:
        assert conn.execute("SELECT count(*) FROM volume_read_holds WHERE task=?", (task_id,)).fetchone() == (0,)
        assert conn.execute("SELECT holder FROM holds WHERE volume=?",
                            (volume["resource_id"],)).fetchone() == (app_id,)
    print("PASS an external Docker restart blocked the read and released its reader hold")


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
    read = node.wait(node.submit("volume.files", {"target": target(old_volume["reference"])}))
    assert (read["lifecycle"], read["conditions"][0]["type"]) == ("Failed", "Conflict"), read
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

    volume, agent = agent_volume(node)
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
    verify_shared_reads(node, volume)
    verify_agent_read(node, volume, agent)

    node.stop(signal.SIGKILL)
    offline = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert offline["database_format"] >= 140 and offline["sqlite_integrity"] == "Verified", offline
    node.start()
    verify_shared_reads(node, volume)
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
    assert "resource_usage" not in observed, observed
    assert verify_kernel(DOCKER, observed, data, volume, access="ReadOnly")["mount_id"] == evidence["mount_id"]
    measured = delegated("application.inspect_instance", {**target(app), "include_usage": True})
    verify_cgroup_usage(measured)
    assert (measured["container_id"], measured["started_at"]) == (observed["container_id"], observed["started_at"])
    app = delegated("application.restart", {**target(app), "expected_boot_id": observed["boot_id"],
                                             "expected_started_at": observed["started_at"]})
    after, same_attachment = verify_application(node, app, data, volume)
    assert after["container_id"] == evidence["container_id"] and after["started_at"] != evidence["started_at"]
    assert same_attachment == attachment
    verify_shared_reads(node, volume)
    restarted = delegated("application.inspect_instance", {**target(app), "include_usage": True})
    verify_cgroup_usage(restarted)
    assert restarted["started_at"] == after["started_at"] and restarted["started_at"] != measured["started_at"]
    print("PASS exact OCI cgroup v2 usage before and after an authorized restart")
    app = node.perform("application.stop", target(app))
    detached = node.get(attachment["resource_id"])["value"]
    assert (detached["effective_access"], detached["lifecycle"]) == ("Blocked", "Detached"), detached

    staged, _, _, _ = stage_from_artifact(node, app, dict(spec, release_version="2.0.0"))
    assert release_mode(node, staged) == "ReadOnly"
    app = node.perform("application.select_release", {"target": target(app), "release_ref": staged})
    volume = node.get(volume["resource_id"])["reference"]
    app = recover_unsettled_start(node, app, data, volume)
    verify_shared_reads(node, volume)
    app = node.perform("application.stop", target(app))
    node.perform("application.uninstall", target(app))
    assert (data / "seed").read_bytes() == b"original" and not (data / "new").exists()
    volume = node.get(volume["resource_id"])["reference"]
    volume = node.perform("volume.write", {**target(volume), "name": "new", "content": "after detach"})["volume_ref"]
    assert (data / "new").read_bytes() == b"after detach"
    refuse_other_invocation(node, spec, volume, app_ids)
    node.stop()
    final = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert (final["witness"], final["sqlite_integrity"]) == ("Confirmed", "Verified"), final
    print("PASS shared file reads across Core and approved OCI restarts, signed recovery, detach and offline verification")


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
