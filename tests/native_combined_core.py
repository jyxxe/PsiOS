"""Exercise kernel evidence, task waits and backup export on one native disk."""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, password
from native_static_ipv4 import installer, poweroff


def submit(console, tool, parameters, *, delegation=None):
    payload = shlex.quote(json.dumps(parameters))
    delegated = f" --delegation {shlex.quote(delegation)}" if delegation else ""
    result = json.loads(console.run(f"printf '%s' {payload} | psiosctl submit {tool}{delegated} --key native-{tool}"))
    return result["task_id"]


def artifact(console, task):
    finished = call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})
    assert finished["lifecycle"] == "Succeeded", finished
    reference, = finished["result"]["artifact_refs"]
    return call(console, "Artifact.Read", {"reference": reference})


def scope(console):
    node = call(console, "Node.Get", {})["reference"]["resource_id"]
    grant = call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node], "units": 4,
        "ttl_seconds": 180, "actions": ["task.submit", "task.read", "node.block_layout", "node.pressure"]})
    return node, grant["delegation_ref"]["resource_id"]


def observations(console):
    node, delegation = scope(console)
    disk = console.run("cat /sys/class/block/vda/dev")
    sequence = console.run("cat /sys/class/block/vda/diskseq")
    report = artifact(console, submit(console, "node.block_layout", {"major_minor": disk}, delegation=delegation))
    assert report["node_ref"]["resource_id"] == node
    assert report["fence"]["disk_sequence"] == sequence and report["layout"]["major_minor"] == disk
    assert [part["number"] for part in report["layout"]["partitions"]] == [1, 2, 3], report
    pressure = artifact(console, submit(console, "node.pressure", {}, delegation=delegation))
    assert pressure["node_ref"]["resource_id"] == node and pressure["boot_id"] == report["fence"]["boot_id"]
    for resource in ("cpu", "memory", "io"):
        path = f"/proc/pressure/{resource}"
        sample = console.run(f"if [ -r {path} ]; then cat {path}; fi")
        reading = pressure["kernel"][resource]
        if sample:
            totals = {line.split()[0]: int(line.rsplit("total=", 1)[1]) for line in sample.splitlines()}
            assert reading["status"] == "Observed" and reading["value"]["some"]["total_us"] <= totals["some"], reading
        else:
            assert reading == {"status": "Unsupported"}, reading


def wait_through_cordon(console):
    node = call(console, "Node.Get", {})["reference"]
    cordoned = call(console, "Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
    task = submit(console, "system.status", {})
    paused = call(console, "Task.Wait", {"id": task, "timeout_ms": 75})
    assert paused["lifecycle"] == "Queued", paused
    call(console, "Node.Configure", {"expected_resource_version": cordoned["version"], "scheduling": "Enabled"})
    assert artifact(console, task)["authority_id"] == node["resource_id"]


def recover_backup(console):
    volume = artifact(console, submit(console, "volume.create", {"name": "native-restore"}))
    root = shlex.quote(f"/var/lib/psios/volumes/{volume['resource_id']}/data")
    console.run(f"cd {root} && umask 077 && printf 'persistent native backup' > record && sync -f .")
    params = {"id": volume["resource_id"], "expected_resource_version": volume["version"]}
    backup = artifact(console, submit(console, "backup.create", params))
    bid = backup["resource_id"]
    call(console, "Grant.Issue", {"principal_id": "local-owner", "actions": ["backup.export"],
        "tool_targets": [bid], "ttl_seconds": 120, "units": 1})
    reference = shlex.quote(json.dumps(backup))
    exported = json.loads(console.run(f"printf '%s' {reference} | psiosctl backup-export --output ./detached"))
    assert exported["backup_ref"] == backup and exported["source_ref"] == volume, exported
    console.run("mkdir -m 700 ./recovery")
    report = json.loads(console.run("psiosd --config /tmp/absent-authority.json verify-backup --directory ./detached "
        f"--manifest-digest {shlex.quote(exported['manifest_digest'])} --restore-to ./recovery/data"))
    assert report["source_ref"] == volume and report["backup_id"] == bid, report
    assert console.run("cat recovery/data/record") == "persistent native backup"
    assert console.run("stat -c '%a' recovery/data/record") == "600"


def exercise(args, work):
    user, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    for name, destination in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / name, work / destination)
    disk = work / "combined.raw"
    subprocess.run([*installer(args, disk, args.archive), "--console-hash", operator_hash,
                    "--root-console-hash", root_hash], check=True, stdout=subprocess.DEVNULL, timeout=140)
    with boot(work, disk, pace_commands=True) as console:
        console.login("psios", user)
        observations(console)
        wait_through_cordon(console)
        recover_backup(console)
        console.send("exit")
        console.login("root", root)
        poweroff(console)
    check_clean_shutdown(disk)
    print("PASS block layout, pressure, durable Task.Wait, backup export, detached recovery and clean shutdown in one native boot")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-combined-") as directory:
        exercise(parser.parse_args(), Path(directory))
