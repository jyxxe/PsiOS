"""Enforce Core's configured cgroup limits in a real offline UEFI guest."""
import argparse
import importlib.util
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, check_state, password, sha

GROUP = "/sys/fs/cgroup/psios-core"
LIMITS = {"cpu_millicores": 250, "memory_mib": 96, "pids_max": 24}
FLAGS = ["--core-cpu-millicores", "250", "--core-memory-mib", "96", "--core-pids-max", "24"]


def counter(console, file, name):
    value = console.run(f"awk '$1==\"{name}\" {{ print $2 }}' {GROUP}/{file}")
    # Kernel OOM output and the supervised service also write to the serial port.
    count, = (int(line) for line in value.splitlines() if line.isdecimal())
    return count


def enter(command):
    return f"(printf '%s\\n' 0 > {GROUP}/cgroup.procs && exec {command})"


def wait_restart(console, old_pid):
    console.run("for i in $(seq 1 150); do pid=$(pidof psiosd) && "
                f'[ "$pid" != {old_pid} ] && [ "$(cat /proc/$pid/cgroup)" = 0::/psios-core ] && '
                "su -s /bin/sh psios -c 'psiosctl whoami' >/dev/null 2>&1 && exit 0; sleep 0.1; done; exit 1")


def check_limits(console):
    assert console.run("cat /proc/self/cgroup /proc/1/cgroup /proc/$(pidof psiosd)/cgroup") == (
        "0::/\n0::/\n0::/psios-core")
    assert console.run(f"stat -c '%u:%g' {GROUP} {GROUP}/cgroup.procs") == "0:0\n0:0"
    for file in ("cpu.max", "memory.max", "memory.swap.max", "pids.max", "cgroup.procs"):
        console.run(f'! (printf "0" > {GROUP}/{file}) 2>/dev/null')
    assert console.run(f"cat {GROUP}/cpu.max {GROUP}/memory.max {GROUP}/memory.swap.max {GROUP}/pids.max") == (
        "25000 100000\n100663296\n0\n24")
    node = call(console, "Node.Get", {})["reference"]["resource_id"]
    grant = call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node], "units": 3,
        "ttl_seconds": 120, "actions": ["node.pressure", "task.submit", "task.read"]})
    delegated = shlex.quote(grant["delegation_ref"]["resource_id"])
    task = json.loads(console.run(f"printf '{{}}' | psiosctl submit node.pressure --delegation {delegated} --key {delegated}"))
    outcome = json.loads(console.run("psiosctl wait " + task["task_id"]))
    assert outcome["lifecycle"] == "Succeeded", outcome
    reference, = outcome["result"]["artifact_refs"]
    report = call(console, "Artifact.Read", {"reference": reference})["core_cgroup"]
    assert report["status"] == "Observed", report
    group = report["value"]
    assert group["source"] == "LinuxCgroupV2" and group["inode"] == console.run(f"stat -c '%i' {GROUP}")
    assert group["cpu"]["configured_limit"]["value"] == {
        "period_us": 100000, "quota_us": {"kind": "Bounded", "value": 25000}}
    for category, name, value in (("memory", "configured_max_bytes", 100663296),
                                   ("memory", "configured_swap_max_bytes", 0), ("pids", "configured_max", 24)):
        assert group[category][name] == {"status": "Observed", "value": {"kind": "Bounded", "value": value}}


def load_core(console):
    before = counter(console, "cpu.stat", "nr_throttled")
    burner = enter("yes")
    console.run(f'{burner} >/dev/null & pid=$!; sleep 1; kill "$pid"; wait "$pid" || :')
    assert counter(console, "cpu.stat", "nr_throttled") > before
    print("Observed CPU throttling inside the Core cgroup", flush=True)

    before = counter(console, "pids.events", "max")
    child = "i=0; while [ $i -lt 32 ]; do sleep 2 & i=$((i+1)); done; wait"
    console.run(f'{enter("sh -c " + shlex.quote(child))} >/dev/null 2>&1 || :')
    assert counter(console, "pids.events", "max") > before
    print("Observed kernel rejection at the Core PID limit", flush=True)

    daemon = console.run("pidof psiosd")
    before = counter(console, "memory.events.local", "oom_group_kill")
    script = shlex.quote('BEGIN { s="test"; for(i=0;i<25;i++) s=s s; print length(s) }')
    console.run(f'{enter("awk " + script)} >/dev/null 2>&1; [ "$?" -eq 137 ]')
    assert counter(console, "memory.events.local", "oom_group_kill") > before
    wait_restart(console, daemon)
    print("Observed group OOM and restarted Core in the same cgroup", flush=True)

    daemon = console.run("pidof psiosd")
    orphan = console.run(f"{enter('sleep 45')} </dev/null >/dev/null 2>&1 & echo $!")
    console.run(f"for i in $(seq 1 30); do grep -qx {orphan} {GROUP}/cgroup.procs && exit 0; sleep 0.1; done; exit 1")
    console.run(f"kill -KILL {daemon}")
    wait_restart(console, daemon)
    console.run(f"! grep -qx {orphan} {GROUP}/cgroup.procs")

    daemon = console.run("pidof psiosd")
    console.run(f"echo 'max 100000' > {GROUP}/cpu.max; kill -KILL {daemon}; sleep 4; ! pidof psiosd")
    console.run(f"echo '25000 100000' > {GROUP}/cpu.max")
    wait_restart(console, daemon)


def refuse_unavailable_controller(args, work, disk, receipt):
    # Disable a real kernel controller while retaining the sealed profile.
    stage = work / "incomplete-kernel"
    stage.mkdir()
    spec = importlib.util.spec_from_file_location("native_installer", args.installer)
    installer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(installer)
    inputs = argparse.Namespace(kernel_iso=args.kernel_iso, core_archive=args.archive, core_sha256=sha(args.archive))
    installer.ramfs(stage, inputs, receipt["state_uuid"], receipt["home_uuid"],
                    core_limits=installer.CoreLimits(**LIMITS))
    efi = installer.boot_media.efi_application(stage, installer.CMDLINE + " cgroup_disable=memory")
    subprocess.run(["mcopy", "-o", "-i", f"{disk}@@1048576", efi, "::/EFI/BOOT/BOOTX64.EFI"], check=True, timeout=20)
    before = sha(disk)
    with boot(work, disk) as console:
        console.until(b"PsiOS boot stopped at core-resource-limits")
        assert console.vm.wait(25) == 0
    assert sha(disk) == before


def exercise(args, work):
    user, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", work / "code.fd")
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", work / "vars.fd")
    disk = work / "bounded.raw"
    upgraded = work / "upgraded.raw"
    prior = work / "prior.json"
    installer = [sys.executable, args.installer]
    inputs = ["--core-archive", args.archive, "--core-sha256", sha(args.archive), "--kernel-iso", args.kernel_iso]
    fresh = [*installer, disk, *inputs, "--console-hash", operator_hash, "--root-console-hash", root_hash]
    incomplete = subprocess.run([*fresh, *FLAGS[:2]], capture_output=True, text=True, timeout=10)
    assert incomplete.returncode and "provide all three" in incomplete.stderr and not disk.exists(), incomplete.stderr
    prior.write_bytes(subprocess.check_output([*fresh, *FLAGS], timeout=150))
    receipt = json.loads(prior.read_text())
    assert receipt["format"] == 6 and receipt["core_limits"] == LIMITS
    with boot(work, disk) as console:
        console.login("psios", user)
        assert console.run("sha256sum /psios-deployment.json").split()[0] == receipt["deployment_manifest_sha256"]
        assert json.loads(console.run("cat /psios-deployment.json"))["core_limits"] == LIMITS
        check_limits(console)
        initial = check_state(console)
        console.send("exit")
        console.login("root", root)
        load_core(console)
        console.send("cd /; exec poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(disk)

    upgrade = [*installer, upgraded, *inputs, "--upgrade-from", disk, "--previous-receipt", prior]
    unchanged = sha(disk)
    override = subprocess.run([*upgrade, *FLAGS], capture_output=True, text=True, timeout=10)
    assert override.returncode and "preserves Core resource limits" in override.stderr and not upgraded.exists()
    next_receipt = json.loads(subprocess.check_output(upgrade, timeout=310))
    assert next_receipt["format"] == 6 and next_receipt["core_limits"] == LIMITS
    assert next_receipt["upgrade"]["previous_image_sha256"] == sha(disk) == unchanged
    with boot(work, upgraded) as console:
        console.login("psios", user)
        after = check_state(console)
        assert after[0] == initial[0] and after[1]["task_id"] == initial[1]["task_id"] and after[2] == initial[2]
        check_limits(console)
        console.send("exit")
        console.login("root", root)
        console.send("cd /; exec poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(upgraded)
    refuse_unavailable_controller(args, work, upgraded, next_receipt)
    print("PASS native CPU throttling, PID and OOM enforcement, supervised recovery and limit-preserving upgrade")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-core-limits-") as directory:
        exercise(parser.parse_args(), Path(directory))
