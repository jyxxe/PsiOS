"""Use real native Linux hotplug to advance an enrolled block device revision."""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, password, sha
from native_upgrade import shutdown
from native_usb_inventory import monitor


def task_result(console, tool, key, delegation, params):
    payload = shlex.quote(json.dumps(params, separators=(",", ":")))
    receipt = json.loads(console.run(f"printf '%s' {payload} | psiosctl submit {tool} --key {key} "
                                     f"--delegation {shlex.quote(delegation)}"))
    task = call(console, "Task.Wait", {"id": receipt["task_id"], "timeout_ms": 6000})
    assert task["lifecycle"] == "Succeeded", task
    return call(console, "Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    for source, target in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / target)
    disk = work / "native.raw"
    subprocess.run([sys.executable, "-B", args.installer, disk, "--kernel-iso", args.kernel_iso,
                    "--core-archive", args.archive, "--core-sha256", sha(args.archive),
                    "--console-hash", operator_hash, "--root-console-hash", admin_hash],
                   check=True, stdout=subprocess.DEVNULL, timeout=180)
    with boot(work, disk, pace_commands=True) as console:
        console.login("root", admin)
        console.run("sed -i 's/^{/{\"block_device_reconcile_ms\":60000,/' /etc/psios/core.json && sync -f /etc")
        console.send("cd /; exec poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(disk)

    medium = work / "detachable.raw"
    with medium.open("xb") as image:
        image.truncate(16 * 1024 * 1024)
    medium_hash = sha(medium)
    with boot(work, disk, block_hotplug=True, pace_commands=True) as console:
        console.login("psios", operator)
        node = call(console, "Node.Get", {})["reference"]["resource_id"]
        issued = call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                      "units": 4, "ttl_seconds": 120,
                      "actions": ["node.block_layout", "node.track_block_device", "task.submit", "task.read"]})
        delegation = issued["delegation_ref"]["resource_id"]
        with monitor(work / "block-control.sock") as qmp:
            qmp("blockdev-add", driver="file", filename=str(medium), **{"node-name": "wake-file", "read-only": True})
            qmp("blockdev-add", driver="raw", file="wake-file", **{"node-name": "wake-raw", "read-only": True})
            qmp("device_add", driver="virtio-blk-pci", drive="wake-raw", serial="PSIOS-WAKE",
                bus="psios-block-port", id="wake-device")
            device = console.run(
                "for i in $(seq 1 100); do for b in /sys/class/block/vd*; do "
                'test -r "$b/serial" && test "$(cat "$b/serial")" = PSIOS-WAKE && '
                'printf "%s\\n" "${b##*/}" && exit 0; done; sleep 0.1; done; exit 1')
            devnum = console.run(f"cat /sys/class/block/{device}/dev")
            layout = task_result(console, "node.block_layout", "wake-layout", delegation,
                                 {"major_minor": devnum, "trackable": True})
            enrolled = task_result(console, "node.track_block_device", "wake-enroll", delegation,
                                   {"major_minor": devnum, "expected": layout["fence"]})
            reference = enrolled["device_ref"]
            assert enrolled["lifecycle"] == "Present", enrolled
            identifier = shlex.quote(reference["resource_id"])
            snapshot = json.loads(console.run(f"psiosctl list BlockDevice --id {identifier}"))
            assert snapshot["items"] == [reference], snapshot
            qmp("device_del", id="wake-device")
            stream = shlex.quote(snapshot["watch_start_cursor"])
            event_page = json.loads(console.run(f"psiosctl watch {stream} --wait-ms 8000"))
            assert float(console.run("cut -d ' ' -f1 /proc/uptime")) < 55, event_page
            assert not console.run(f"test ! -e /sys/class/block/{device}")
            event, = event_page["events"]
            receipt = call(console, "BlockDevice.Get", {"id": reference["resource_id"]})
            assert event["resource_ref"] == receipt["reference"], (event, receipt)
            assert receipt["state"]["lifecycle"] == "Absent" and receipt["reference"]["version"] == "r2", receipt
        shutdown(console, admin)
    check_clean_shutdown(disk)
    assert sha(medium) == medium_hash
    print("PASS actual kernel block removal woke the scoped sysfs authority before its sixty-second check")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for flag in ("--kernel-iso", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-block-wake-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
