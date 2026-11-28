"""Rebuild the real native boot payload and unsigned EFI with matched deployment inputs."""
import argparse
from importlib.util import module_from_spec, spec_from_file_location
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import uuid

from native_disk import REPO, sha


def check_embedded_apks(work, manifest, expected):
    packages = manifest["embedded_apks"]
    assert set(packages) == set(expected), packages
    for name, record in packages.items():
        raw = subprocess.check_output(["isoinfo", "-R", "-i", work / "base.iso", "-x",
                                       f"/apks/x86_64/{name}.apk"], timeout=30)
        assert hashlib.sha256(raw).hexdigest() == record["sha256"], name
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz", ignore_zeros=True) as archive:
            with archive.extractfile(".PKGINFO") as entry:
                metadata = dict(line.split(" = ", 1) for line in entry.read().decode().splitlines() if " = " in line)
        assert metadata["pkgname"] + "-" + metadata["pkgver"] == name, name
        assert (metadata["license"], metadata["commit"], metadata["url"]) == (
            record["declared_license"], record["aports_commit"], record["upstream"]), name


def build(args, installer, build_time, mask):
    old = os.umask(mask)
    try:
        with tempfile.TemporaryDirectory(prefix="psios-native-repro-") as directory:
            work = Path(directory)
            options = {"wired_driver": "e1000e", "ssh_enabled": True} if args.ssh_e1000e else {}
            network = "e1000e-dhcp" if args.ssh_e1000e else None
            if args.rdnss:
                network = installer.RDNSS
            base = installer.prepare_boot_inputs(work, args, network_profile=network, **options)
            files = [*installer.core_programs(work), "psios-source"]
            if args.ssh_e1000e:
                files.extend((installer.E1000E_MODULE, *installer.SSH_PROGRAMS, "usr/lib/ssh"))
            if args.rdnss:
                files.append(installer.RDNSS_PROGRAM)
            for name in files:
                os.utime(work / name, (build_time, build_time))
            state = uuid.UUID("4f5f8529-f867-4a80-832d-da687cb5a2f3")
            home = uuid.UUID("eda44a53-e2cb-4932-b9b0-cd0ec15c5ab6")
            installer.assemble_ramfs(work, args, base, state, home, installer.Layout(), None, network,
                                     core_limits=None, trial=None, **options)
            packages = [installer.TZDATA]
            if args.ssh_e1000e:
                packages.extend(installer.SSH_PACKAGES)
            if args.rdnss:
                packages.extend(installer.RDNSS_PACKAGES)
            check_embedded_apks(work, json.loads((work / "psios-deployment.json").read_bytes()), packages)
            return {"deployment manifest": sha(work / "psios-deployment.json"),
                    "initramfs": sha(work / "initrd"),
                    "unsigned EFI": sha(installer.boot_media.efi_application(work, installer.CMDLINE))}
    finally:
        os.umask(old)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", dest="core_archive", type=Path, required=True)
    profile = parser.add_mutually_exclusive_group()
    profile.add_argument("--ssh-e1000e", action="store_true")
    profile.add_argument("--rdnss", action="store_true")
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    args = parser.parse_args()
    args.core_sha256 = sha(args.core_archive)
    spec = spec_from_file_location("native_repro_installer", args.installer)
    installer = module_from_spec(spec)
    spec.loader.exec_module(installer)
    first = build(args, installer, 1_661_000_000, 0o022)
    second = build(args, installer, 1_785_000_000, 0o077)
    for name, digest in first.items():
        print(f"{name}: {digest} {second[name]}", flush=True)
    assert first == second, "matching native boot inputs produced different artifacts"
