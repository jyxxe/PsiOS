"""Exercise sealed, authenticated native maintenance after a real Core boot failure."""
import argparse
import json
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile
import uuid

from native_disk import REPO, boot, check_clean_shutdown, check_state, extract_partition, password, replace_partition, sha
from native_upgrade import STATE_MIB, STATE_OFFSET, shutdown


def update_modes(work, disk, fields):
    partition = work / "maintenance-state.ext4"
    extract_partition(disk, partition, STATE_OFFSET, STATE_MIB)
    edits = "".join(f"set_inode_field {name} mode {mode}\n" for name, mode in fields.items())
    subprocess.run(["debugfs", "-w", "-f", "-", partition], input=edits, text=True, capture_output=True,
                   check=True, timeout=20)
    replace_partition(disk, partition, STATE_OFFSET)
    partition.unlink()


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    signed = args.efi_stub is not None
    if signed:
        from native_secure_boot import check_firmware, create_signer, secure_vm, signing_args

        key, cert = create_signer(work, "maintenance")
        vendor = str(uuid.uuid4())
        subprocess.run(["virt-fw-vars", "-i", args.ovmf_dir / "OVMF_VARS_4M.fd", "-o", work / "enrolled.fd",
                        "--set-pk", vendor, cert, "--add-kek", vendor, cert, "--add-db", vendor, cert, "--sb"],
                       check=True, timeout=40)
        shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.secboot.fd", work / "code.fd")
        guest = secure_vm
        signing = signing_args(args, key, cert)
    else:
        shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", work / "code.fd")
        shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", work / "vars.fd")
        guest = boot
        signing = []
    disk = work / "maintenance.raw"
    build = [sys.executable, "-B", args.installer, disk, "--kernel-iso", args.kernel_iso,
             "--core-archive", args.archive, "--core-sha256", sha(args.archive),
             "--console-hash", operator_hash, "--maintenance-console"]
    refused = subprocess.run(build, capture_output=True, text=True, timeout=15)
    assert refused.returncode == 2 and "requires --root-console-hash" in refused.stderr and not disk.exists()
    subprocess.run([*build, "--root-console-hash", admin_hash, *signing], check=True, timeout=160, stdout=subprocess.DEVNULL)

    with guest(work, disk, pace_commands=True) as console:
        console.login("psios", operator)
        baseline = check_state(console)
        console.send("exit")
        console.login("root", admin)
        assert console.run("stat -c '%u:%g:%a' /etc/psios/maintenance-console") == "0:0:444"
        console.run("printf 'trusted user data' > /root/fixture && printf 'echo persisted-profile' > /root/.profile")
        console.send("poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(disk)

    update_modes(work, disk, {"/psios": "040000"})
    with guest(work, disk, pace_commands=True) as console:
        console.until(b"PsiOS local maintenance: Core authority unavailable; persistent volumes mounted read-only")
        console.credentials("root", "wrong-" + secrets.token_hex(10))
        console.until(b"Login incorrect")
        console.credentials("psios", operator)
        console.until(b"Login incorrect")
        console.login("root", admin)
        assert console.run("id -u") == "0"
        if signed:
            check_firmware(console)
        console.run("! pidof psiosd && test ! -S /run/psios/api.sock && test ! -e /root/.profile")
        record = json.loads(console.run("cat /var/lib/.psios-boot-failure"))
        assert record["format"] == 1 and record["stage"] == "authority"
        assert record["boot_id"] == console.run("cat /proc/sys/kernel/random/boot_id")
        assert console.run("cat /mnt/psios-homes/root/fixture") == "trusted user data"
        for mountpoint in ("/var/lib", "/mnt/psios-homes"):
            refusal = console.run(f"touch {mountpoint}/maintenance-write 2>&1; test $? -ne 0")
            assert "Read-only file system" in refusal, refusal
        console.send("poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(disk)

    update_modes(work, disk, {"/etc/psios/maintenance-console": "0100644"})
    with guest(work, disk) as console:
        console.until(b"PsiOS boot stopped at authority; use external rescue media",
                      reject=b"PsiOS local maintenance:")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(disk)

    update_modes(work, disk, {"/etc/psios/maintenance-console": "0100444", "/psios": "040700"})
    with guest(work, disk, pace_commands=True) as console:
        console.login("psios", operator)
        node, receipt, result, boot_id = check_state(console)
        assert node == baseline[0] and receipt["task_id"] == baseline[1]["task_id"] and result == baseline[2]
        assert receipt["deduplicated"] and boot_id not in (record["boot_id"], baseline[3])
        shutdown(console, admin)
    check_clean_shutdown(disk)
    print("PASS authenticated native maintenance, read-only state and home, safe refusal and recovery of the same Core Task")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--efi-stub", type=Path, help="also require actual enrolled OVMF Secure Boot")
    for flag in ("--kernel-iso", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-maintenance-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
