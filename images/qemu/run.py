#!/usr/bin/env python3
"""Start a prepared PsiOS guest with its own disk, seed and firmware state."""
import argparse
import hashlib
import json
import os
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("instance", type=Path)
    parser.add_argument("--ssh-port", type=int, default=2222)
    parser.add_argument("--accel", choices=["auto", "kvm", "tcg"], default="auto")
    parser.add_argument("--serial-log", type=Path, help="write serial output to this file instead of the terminal")
    args = parser.parse_args()
    if not 1 <= args.ssh_port <= 65535:
        parser.error("SSH port must be between 1 and 65535")
    serial = "file:" + str(args.serial_log.resolve()) if args.serial_log else "stdio"
    os.chdir(args.instance)
    manifest = json.loads(Path("manifest.json").read_text())
    if manifest["architecture"] != "amd64":
        parser.error("expected an amd64 instance")
    firmware = []
    if manifest["firmware"] == "OVMF":
        with Path("uefi-code.fd").open("rb") as code:
            if hashlib.file_digest(code, "sha256").hexdigest() != manifest["ovmf"]["code_sha256"]:
                parser.error("OVMF code differs from the prepared instance")
        firmware = ["-machine", "q35", "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=uefi-code.fd",
                    "-drive", "if=pflash,format=raw,unit=1,file=uefi-vars.fd"]
    elif manifest["firmware"] != "SeaBIOS":
        parser.error("unknown instance firmware")
    accel = args.accel
    if accel == "auto":
        accel = "kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    command = ["qemu-system-x86_64", "-accel", accel, "-cpu", "host" if accel == "kvm" else "max",
               "-m", "1536", "-smp", "2", "-display", "none", "-monitor", "none", "-serial", serial, *firmware,
               "-drive", "file=system.qcow2,format=qcow2,if=virtio",
               "-drive", "file=seed.iso,format=raw,media=cdrom,readonly=on",
               "-nic", f"user,model=virtio-net-pci,restrict=on,hostfwd=tcp:127.0.0.1:{args.ssh_port}-:22"]
    os.execvp(command[0], command)


if __name__ == "__main__":
    main()
