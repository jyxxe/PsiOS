"""Observe actual native USB hotplug through authorized, paginated Core Tasks."""
import argparse
from contextlib import contextmanager
import json
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, password, sha
from native_upgrade import shutdown


@contextmanager
def monitor(path):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(15)
        connection.connect(str(path))
        with connection.makefile("rb") as incoming:
            def receive():
                line = incoming.readline(64 * 1024)
                assert line.endswith(b"\n"), "incomplete QMP message"
                return json.loads(line)

            def execute(name, **arguments):
                request = {"execute": name, "arguments": arguments, "id": name}
                connection.sendall(json.dumps(request).encode("ascii") + b"\r\n")
                for _ in range(64):
                    message = receive()
                    if "event" in message:
                        continue
                    assert message == {"return": {}, "id": name}, message
                    return
                raise AssertionError(f"no QMP response for {name}")

            assert "QMP" in receive()
            execute("qmp_capabilities")
            yield execute


def observe(console, key, delegation, parameters, *, error=None):
    payload = shlex.quote(json.dumps(parameters, separators=(",", ":")))
    enrolled = shlex.quote(delegation)
    accepted = json.loads(console.run(f"printf '%s' {payload} | psiosctl submit node.usb --key {key} --delegation {enrolled}"))
    outcome = call(console, "Task.Wait", {"id": accepted["task_id"], "timeout_ms": 5000})
    if error:
        assert outcome["lifecycle"] == "Failed" and outcome["conditions"][0]["type"] == error, outcome
        return None
    assert outcome["lifecycle"] == "Succeeded", outcome
    reference, = outcome["result"]["artifact_refs"]
    return reference, call(console, "Artifact.Read", {"reference": reference})


def tablet(console):
    return console.run("for i in $(seq 1 100); do for entry in /sys/bus/usb/devices/*; do "
                       'test -r "$entry/idVendor" && test "$(cat "$entry/idVendor")" = 0627 && '
                       'printf "%s\\n" "${entry##*/}" && exit 0; done; sleep 0.1; done; exit 1')


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

    with boot(work, disk, pace_commands=True, usb_control=True) as console:
        console.login("root", admin)
        console.run("modprobe xhci_pci && test -d /sys/bus/usb/devices")
        console.send("exit")
        console.login("psios", operator)
        missing = console.run("printf '{}' | psiosctl submit node.usb --key usb-without-grant 2>&1; test $? -ne 0")
        assert "PermissionDenied" in missing, missing
        node = call(console, "Node.Get", {})["reference"]["resource_id"]

        def grant(units):
            issued = call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                          "units": units, "ttl_seconds": 300, "actions": ["node.usb", "task.submit", "task.read"]})
            return issued["delegation_ref"]["resource_id"]

        delegation = grant(12)
        fixture, before = observe(console, "usb-before", delegation, {"limit": 1})
        cursor = before["next_cursor"]
        assert cursor and before["devices"][0]["location"].startswith("usb"), before
        _, settled = observe(console, "usb-same", delegation, {"limit": 1, "cursor": cursor})
        assert settled["snapshot_digest"] == before["snapshot_digest"], settled
        observe(console, "usb-other-grant", grant(2), {"limit": 1, "cursor": cursor}, error="ResyncRequired")

        with monitor(work / "usb-control.sock") as qmp:
            qmp("device_add", driver="usb-tablet", id="tablet-first", bus="psios-usb.0", port="1")
            location = tablet(console)
            observe(console, "usb-hotplug-old", delegation, {"limit": 1, "cursor": cursor}, error="ResyncRequired")
            retained, inserted = observe(console, "usb-inserted", delegation, {"limit": 1})
            attached = inserted["devices"][0]
            assert attached["location"] == location and attached["vendor_id"] == "0627" and attached["authorized"], inserted
            assert inserted["boot_id"] == before["boot_id"] and inserted["next_cursor"], inserted
            qmp("device_del", id="tablet-first")
            console.run(f"for i in $(seq 1 100); do test ! -e /sys/bus/usb/devices/{location} && exit 0; "
                        "sleep 0.1; done; exit 1")
            qmp("device_add", driver="usb-tablet", id="tablet-replacement", bus="psios-usb.0", port="1")
            assert tablet(console) == location
            observe(console, "usb-replaced-old", delegation, {"limit": 1, "cursor": inserted["next_cursor"]},
                    error="ResyncRequired")
            _, replacement = observe(console, "usb-replacement", delegation, {"limit": 1})
            current, = replacement["devices"]
            for key in ("location", "parent_location", "vendor_id", "product_id", "bus_number"):
                assert current[key] == attached[key], (key, attached, current)
            assert current["device_address"] != attached["device_address"], (attached, current)
            assert "serial" not in current and "sysfs_identity" not in current
        assert call(console, "Artifact.Read", {"reference": fixture}) == before
        assert call(console, "Artifact.Read", {"reference": retained}) == inserted
        shutdown(console, admin)
    check_clean_shutdown(disk)
    print("PASS observed real USB root hubs, USB attachment, unchanged slot replacement, cursor invalidation and durable artifacts")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for flag in ("--kernel-iso", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-usb-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
