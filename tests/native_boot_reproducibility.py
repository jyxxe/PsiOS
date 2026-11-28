"""Rebuild the real native boot payload and unsigned EFI with matched deployment inputs."""
import argparse
from importlib.util import module_from_spec, spec_from_file_location
import os
from pathlib import Path
import tempfile
import uuid

from native_disk import REPO, sha


def build(args, installer, build_time, mask):
    old = os.umask(mask)
    try:
        with tempfile.TemporaryDirectory(prefix="psios-native-repro-") as directory:
            work = Path(directory)
            base = installer.prepare_boot_inputs(work, args)
            for name in (*installer.core_programs(work), "psios-source"):
                os.utime(work / name, (build_time, build_time))
            state = uuid.UUID("4f5f8529-f867-4a80-832d-da687cb5a2f3")
            home = uuid.UUID("eda44a53-e2cb-4932-b9b0-cd0ec15c5ab6")
            installer.assemble_ramfs(work, args, base, state, home, installer.Layout(), None, None,
                                     core_limits=None, trial=None)
            return {"deployment manifest": sha(work / "psios-deployment.json"),
                    "initramfs": sha(work / "initrd"),
                    "unsigned EFI": sha(installer.boot_media.efi_application(work, installer.CMDLINE))}
    finally:
        os.umask(old)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", dest="core_archive", type=Path, required=True)
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
