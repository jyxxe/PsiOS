"""Measure the authenticated native development profile in an offline QEMU guest."""

import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import time

from native_disk import boot, sha
from native_install_virtio import make_source


def field_kib(text, names):
    values = {}
    for line in text.splitlines():
        field, _, value = line.partition(":")
        if field in names:
            amount, unit = value.split()
            assert unit == "kB" and amount.isdecimal()
            values[field] = int(amount)
    assert set(values) == set(names), values
    return values


def memory(console):
    guest = field_kib(console.run("cat /proc/meminfo"), ("MemTotal", "MemAvailable"))
    core = field_kib(console.run("set -- $(pidof psiosd); test \"$#\" = 1 && cat /proc/$1/status"), ("VmRSS", "VmHWM"))
    return {"guest_total_kib": guest["MemTotal"], "guest_available_kib": guest["MemAvailable"],
            "core_rss_kib": core["VmRSS"], "core_peak_rss_kib": core["VmHWM"]}


def measure(work, image, deployment_digest, operator, admin, number):
    start = time.monotonic_ns()
    with boot(work, image) as console:
        command = console.vm.args
        machine = {field: command[command.index(option) + 1] for field, option in (
            ("machine", "-machine"), ("accelerator", "-accel"), ("vcpus", "-smp"), ("memory", "-m"))}
        console.login("psios", operator)
        session = json.loads(console.run("psiosctl whoami"))
        ready_ms = (time.monotonic_ns() - start) // 1_000_000
        uptime = console.run("cut -d ' ' -f1 /proc/uptime")
        boot_id = console.run("cat /proc/sys/kernel/random/boot_id")
        console_kernel = console.run("uname -r")
        assert session["principal_id"] == "local-owner"
        console.run("test -d /sys/firmware/efi && for n in /sys/class/net/*; do test ! -e \"$n/device\" || exit 1; done")
        assert console.run("sha256sum /psios-deployment.json").split()[0] == deployment_digest
        console.run("test -S /run/psios/api.sock")
        ready_memory = memory(console)
        task_start = time.monotonic_ns()
        task = json.loads(console.run(f"printf '{{}}' | psiosctl submit system.status --key footprint-{number}"))
        assert not task["deduplicated"], task
        result = json.loads(console.run(f"psiosctl wait {task['task_id']}"))
        task_ms = (time.monotonic_ns() - task_start) // 1_000_000
        assert result["lifecycle"] == "Succeeded", result
        task_memory = memory(console)
        console.send("exit")
        console.login("root", admin)
        shutdown_start = time.monotonic_ns()
        console.send("poweroff")
        assert console.vm.wait(35) == 0
        shutdown_ms = (time.monotonic_ns() - shutdown_start) // 1_000_000
    return session["authority_id"], boot_id, machine, {
        "authenticated_core_ready_ms": ready_ms,
        "guest_uptime_after_auth_seconds": float(uptime),
        "guest_kernel": console_kernel,
        "after_auth": ready_memory, "after_workload": task_memory,
        "shell_task_submit_and_wait_ms": task_ms, "orderly_shutdown_ms": shutdown_ms,
        "host_allocated_image_bytes_after_shutdown": image.stat().st_blocks * 512,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--archive-sha256", required=True)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--steady-boots", type=int, default=3, choices=range(1, 9), metavar="1-8")
    args = parser.parse_args()
    if (not re.fullmatch(r"[0-9a-f]{64}", args.archive_sha256)
            or not args.archive.is_file() or sha(args.archive) != args.archive_sha256):
        parser.error("portable archive differs from its expected SHA-256")
    with tempfile.TemporaryDirectory(prefix="psios-native-footprint-") as directory:
        work = Path(directory)
        firmware = {}
        for source, target in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
            shutil.copyfile(args.ovmf_dir / source, work / target)
            firmware[source] = sha(work / target)
        image, receipt, operator, admin = make_source(args, work)
        build = json.loads(receipt.read_bytes())
        initial = image.stat()
        initial_digest = sha(image)
        assert initial_digest == build["initial_image_sha256"]
        deployment = build["deployment_manifest_sha256"]
        authority, first, machine, enrollment = measure(work, image, deployment, operator, admin, 0)
        seen = {first}
        steady = []
        for number in range(1, args.steady_boots + 1):
            current, identity, current_machine, sample = measure(work, image, deployment, operator, admin, number)
            assert current == authority and identity not in seen and current_machine == machine
            seen.add(identity)
            steady.append(sample)
        print(json.dumps({
            "schema_version": 1,
            "source_commit": build["source_commit"], "archive_sha256": args.archive_sha256,
            "kernel_iso_sha256": sha(args.kernel_iso), "firmware_sha256": firmware,
            "qemu": subprocess.check_output(["qemu-system-x86_64", "--version"], text=True).splitlines()[0],
            "host_kernel": platform.release(), "host_architecture": platform.machine(),
            "host_disk_filesystem": subprocess.check_output(["stat", "-f", "-c", "%T", str(work)], text=True).strip(),
            "host_cpu_count_available": len(os.sched_getaffinity(0)),
            "deployment_manifest_sha256": deployment, "qemu_machine": machine,
            "fixture": "offline-no-network, local console, system.status Task",
            "initial_image": {"size_bytes": initial.st_size, "allocated_bytes": initial.st_blocks * 512,
                              "sha256": initial_digest},
            "enrollment_boot": enrollment, "steady_boots": steady,
        }, sort_keys=True, indent=2))


if __name__ == "__main__":
    main()
