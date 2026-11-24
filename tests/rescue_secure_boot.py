"""Verify an exported signed rescue CD and its firmware and read-only boundaries."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import uuid

from native_secure_boot import create_signer, signing_args
from rescue_disk import INSPECT, MIB, digest, image_file
from support import Node


def volume_with_state(work, binaries):
    node = Node(binaries)
    try:
        node.call("Agent.Register", {"name": "signed-rescue-wal"})
        node.stop(signal.SIGKILL)
        assert (node.state / "state.db-wal").stat().st_size > 32
        report = json.loads(subprocess.check_output([*node.daemon, "verify"]))
        tree = work / "source"
        tree.mkdir(mode=0o755)
        shutil.copytree(node.state, tree / "state")
        shutil.copytree(node.witness, tree / "witness")
        volume = work / "authority.ext4"
        image_file(volume, 32 * MIB)
        subprocess.run(["mkfs.ext4", "-q", "-d", tree, volume], check=True)
        return volume, report
    finally:
        node.close()


def reboot_cd(args, work, image, iso, variables, readonly):
    copied = work / "standalone-vars.fd"
    shutil.copyfile(variables, copied)
    code = args.ovmf_dir / "OVMF_CODE_4M.secboot.fd"
    command = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-machine", "q35,smm=on",
               "-global", "driver=cfi.pflash01,property=secure,value=on", "-accel", "tcg", "-m", "768", "-smp", "2",
               "-display", "none", "-monitor", "none", "-nic", "none", "-serial", "null", "-serial", "stdio",
               "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
               "-drive", f"if=pflash,format=raw,unit=1,file={copied}",
               "-boot", "order=d,strict=on", "-cdrom", iso,
               "-drive", f"if=virtio,format=raw,file={image},readonly={'on' if readonly else 'off'}"]
    return subprocess.check_output(command, stdin=subprocess.DEVNULL, timeout=90).replace(b"\r\n", b"\n")


def exercise(args, work):
    key, cert = create_signer(work, "rescue-enrolled")
    owner = str(uuid.uuid4())
    blank = args.ovmf_dir / "OVMF_VARS_4M.fd"
    enrolled = work / "enrolled.fd"
    subprocess.run(["virt-fw-vars", "-i", blank, "-o", enrolled, "--set-pk", owner, cert,
                    "--add-kek", owner, cert, "--add-db", owner, cert, "--sb"], check=True)
    source_vars = digest(enrolled)
    volume, expected = volume_with_state(work, args.bin_dir)
    source_disk = digest(volume)
    core = args.bin_dir / "psiosd"
    cd = work / "verified.iso"
    call = [sys.executable, args.inspector, volume, "--iso", args.kernel_iso, "--core", core,
            "--core-sha256", digest(core), "--uid", str(os.getuid()), "--gid", str(os.getgid()),
            "--state", "/state", "--witness", "/witness", "--ovmf-dir", args.ovmf_dir,
            "--firmware", "uefi", "--ovmf-vars", enrolled, "--save-rescue-iso", cd,
            *signing_args(args, key, cert)]
    verified = json.loads(subprocess.check_output(call, timeout=210))
    evidence = verified.pop("secure_boot")
    assert verified == expected and evidence["status"] == "Enforced", (verified, evidence)
    assert evidence["efi_stub_sha256"] == digest(args.efi_stub)
    assert digest(volume) == source_disk and digest(enrolled) == source_vars
    cd_digest = digest(cd)

    offline = reboot_cd(args, work, volume, cd, enrolled, readonly=True)
    assert b"\nPSIOS-SECURE-BOOT/1\nEnforced\n" in offline
    independent = offline.split(b"\nPSIOS-RESCUE/1\nOK\n", 1)[1]
    recovered, _ = json.JSONDecoder().raw_decode(independent.decode().lstrip())
    assert recovered == verified, recovered
    disabled = reboot_cd(args, work, volume, cd, blank, readonly=True)
    assert disabled.endswith(b"PSIOS-RESCUE/1\nERROR secure-boot-not-enforced\n"), disabled
    writable = reboot_cd(args, work, volume, cd, enrolled, readonly=False)
    assert b"\nPSIOS-SECURE-BOOT/1\nEnforced\n" in writable
    assert writable.endswith(b"PSIOS-RESCUE/1\nERROR block-device\n"), writable
    assert digest(volume) == source_disk and digest(cd) == cd_digest and digest(enrolled) == source_vars
    print("PASS signed standalone rescue CD, firmware enforcement and writable-source refusal")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inspector", type=Path, default=INSPECT)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for name in ("--bin-dir", "--kernel-iso", "--efi-stub"):
        parser.add_argument(name, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-signed-rescue-") as directory:
        exercise(parser.parse_args(), Path(directory))
