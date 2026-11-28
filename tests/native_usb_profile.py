"""Boot the native xHCI observation profile with devices already attached."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, check_state, password, sha
from native_upgrade import shutdown
from native_usb_inventory import observe as observe_node_usb


PROFILE = "XhciInventory"
PERIPHERALS = (
    "-device", "qemu-xhci,id=inventory",
    "-device", "usb-tablet,bus=inventory.0,port=1",
    "-drive", "if=none,id=inventory-media,file=peripheral.raw,format=raw,readonly=on",
    "-device", "usb-storage,drive=inventory-media,bus=inventory.0,port=2",
)
LIMITS = {"cpu_millicores": 500, "memory_mib": 128, "pids_max": 48}


def observe(console, receipt, key):
    saved = json.loads(console.run("cat /psios-deployment.json"))
    assert saved["usb_profile"] == PROFILE and saved["core_limits"] == LIMITS, saved
    assert console.run("sha256sum /psios-deployment.json").split()[0] == receipt["deployment_manifest_sha256"]
    devices = console.run(
        'for i in $(seq 1 100); do '
        'count=0; for dev in /sys/bus/usb/devices/*-*; do '
        'test -r "$dev/idVendor" || continue; count=$((count+1)); done; '
        '[ "$count" = 2 ] && break; sleep 0.1; done; [ "$count" = 2 ] || exit 1; '
        'for hub in /sys/bus/usb/devices/usb*; do '
        '[ "$(cat "$hub/authorized_default")" = 0 ] || exit 1; done; '
        'for dev in /sys/bus/usb/devices/*-*; do '
        'test -r "$dev/idVendor" || continue; '
        '[ "$(cat "$dev/authorized")" = 0 ] || exit 1; '
        'for interface in "$dev":*; do [ ! -e "$interface" ] || exit 1; done; '
        'printf "%s:%s\\n" "$(cat "$dev/idVendor")" "$(cat "$dev/idProduct")"; done'
    ).splitlines()
    assert len(devices) == 2 and devices[0] != devices[1] and any(v.startswith("0627:") for v in devices), devices
    console.run("test ! -e /sys/module/usbhid && test ! -e /sys/module/usb_storage && test ! -e /dev/sda")
    node = call(console, "Node.Get", {})["reference"]["resource_id"]
    grant = call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                 "units": 1, "ttl_seconds": 300, "actions": ["node.usb", "task.submit", "task.read"]})
    _, inventory = observe_node_usb(console, key, grant["delegation_ref"]["resource_id"], {"limit": 16})
    assert inventory["boot_id"] == console.run("cat /proc/sys/kernel/random/boot_id")
    assert inventory["next_cursor"] is None
    peripherals = [row for row in inventory["devices"] if row["parent_location"] is not None]
    assert all(row["authorized"] is False for row in peripherals), inventory
    assert sorted(f"{row['vendor_id']}:{row['product_id']}" for row in peripherals) == sorted(devices), inventory
    return sorted(devices)


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    code, firmware = work / "code.fd", work / "vars.fd"
    shutil.copyfile(Path("/usr/share/OVMF/OVMF_CODE_4M.fd"), code)
    shutil.copyfile(Path("/usr/share/OVMF/OVMF_VARS_4M.fd"), firmware)
    peripheral = work / "peripheral.raw"
    with peripheral.open("xb") as media:
        media.write(b"native read-only USB acceptance fixture\n")
        media.truncate(2 * 1024 * 1024)
    peripheral_digest = sha(peripheral)
    inputs = ["--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive)]
    credentials = ["--console-hash", operator_hash, "--root-console-hash", admin_hash]

    def build(disk, *profile):
        return json.loads(subprocess.check_output([sys.executable, "-B", args.installer, disk,
                                                  *inputs, *credentials, *profile], timeout=180))

    default = work / "default.raw"
    assert "usb_profile" not in build(default)
    with boot(work, default, extra_qemu_args=PERIPHERALS, pace_commands=True) as console:
        console.login("psios", operator)
        console.run("test ! -e /psios-usb-profile && test ! -e /sys/module/usbcore")
        shutdown(console, admin)
    default.unlink()

    original = work / "original.raw"
    receipt = build(original, "--usb-inventory", "--core-cpu-millicores", "500",
                    "--core-memory-mib", "128", "--core-pids-max", "48")
    assert (receipt["format"], receipt["usb_profile"], receipt["core_limits"]) == (12, PROFILE, LIMITS), receipt
    with boot(work, original, extra_qemu_args=PERIPHERALS, pace_commands=True) as console:
        console.login("psios", operator)
        baseline = check_state(console)
        original_devices = observe(console, receipt, "native-xhci-original")
        shutdown(console, admin)
    check_clean_shutdown(original)
    frozen = sha(original)

    previous = work / "previous.json"
    staged = work / "staged"
    invalid_receipts = (
        ({"usb_profile": "Unknown"}, "invalid USB profile in previous build receipt"),
        ({"format": 6}, "USB profile requires build receipt format 12"),
        ({"network_profile": []}, "invalid network profile in previous build receipt"),
    )
    for mutation, diagnostic in invalid_receipts:
        previous.write_text(json.dumps(receipt | mutation))
        result = subprocess.run([sys.executable, "-B", args.installer, staged, *inputs,
                                 "--upgrade-from", original, "--previous-receipt", previous],
                                capture_output=True, text=True, timeout=20)
        assert result.returncode != 0 and diagnostic in result.stderr, result.stderr
        assert not staged.exists(), staged
    previous.write_text(json.dumps(receipt))
    frozen_vars = sha(firmware)
    report = json.loads(subprocess.check_output([
        sys.executable, "-B", args.installer.with_name("stage_update.py"), staged, *inputs,
        "--upgrade-from", original, "--previous-receipt", previous, "--previous-receipt-sha256", sha(previous),
        "--ovmf-code", code, "--ovmf-code-sha256", sha(code),
        "--ovmf-vars", firmware, "--ovmf-vars-sha256", frozen_vars,
    ], timeout=380))
    assert report["status"] == "Staged", report
    assert sha(original) == frozen and sha(firmware) == frozen_vars

    candidate = staged / "candidate.raw"
    updated = json.loads((staged / "candidate-build.json").read_bytes())
    assert (updated["format"], updated["usb_profile"], updated["core_limits"]) == (12, PROFILE, LIMITS), updated
    shutil.copyfile(firmware, work / "candidate-vars.fd")
    with boot(work, candidate, vars_file="candidate-vars.fd", extra_qemu_args=PERIPHERALS,
              pace_commands=True) as console:
        console.login("psios", operator)
        after = check_state(console)
        assert after[0] == baseline[0] and after[1]["task_id"] == baseline[1]["task_id"], after
        assert after[2] == baseline[2] and after[3] != baseline[3], after
        assert observe(console, updated, "native-xhci-updated") == original_devices
        shutdown(console, admin)
    check_clean_shutdown(candidate)
    assert sha(original) == frozen and sha(firmware) == frozen_vars
    assert sha(peripheral) == peripheral_digest
    print("PASS native PCI xHCI inventory enumerates unconfigured USB devices across offline staging")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-usb-profile-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
