#!/usr/bin/env python3
"""Build a new per-instance PsiOS UEFI development disk. Works without mounts or root."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import uuid

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "rescue"))
import boot_media

MIB = 1024 * 1024
CMDLINE = "console=ttyS0,115200 quiet panic=1 rdinit=/psios-init"


def install_core(work, archive, digest):
    package = work / "core.tar.gz"
    boot_media.pinned_copy(archive, package, digest, 64 * MIB)
    members = {"psiosd": "bin/psiosd", "psiosctl": "bin/psiosctl", "source-commit": "psios-source"}
    found = set()
    total = 0
    with tarfile.open(package, "r|gz") as bundle:
        for index, entry in enumerate(bundle):
            total += entry.size
            if index >= 128 or total > 128 * MIB or entry.name in found:
                raise ValueError("invalid Core archive layout")
            if entry.name not in members:
                continue
            if not entry.isfile() or not 0 < entry.size <= 64 * MIB:
                raise ValueError("invalid Core archive member")
            found.add(entry.name)
            path = work / members[entry.name]
            with path.open("xb") as dst:
                shutil.copyfileobj(bundle.extractfile(entry), dst)
            path.chmod(0o755 if entry.name != "source-commit" else 0o644)
    if found != members.keys() or not re.fullmatch(r"[0-9a-f]{40}\n", (work / "psios-source").read_text()):
        raise ValueError("incomplete Core archive")


def password_hash(path):
    with boot_media.regular_file(path) as f:
        if os.fstat(f.fileno()).st_mode & 0o077:
            raise ValueError("console hash file must be private")
        password = f.read(257)
    if not re.fullmatch(rb"\$6\$[./A-Za-z0-9]{1,16}\$[./A-Za-z0-9]{86}\n?", password):
        raise ValueError("expected one SHA-512 crypt password hash")
    return password.strip()


def host_config(seed, args):
    (seed / "etc/psios").mkdir(parents=True)
    operator = password_hash(args.console_hash)
    root = password_hash(args.root_console_hash) if args.root_console_hash else b"!"
    (seed / "etc/passwd").write_text("root:x:0:0:root:/root:/bin/sh\npsios:x:1000:1000:PsiOS operator:/home/psios:/bin/sh\n")
    (seed / "etc/group").write_text("root:x:0:\npsios:x:1000:\n")
    (seed / "etc/shadow").write_bytes(b"root:" + root + b":0:0:99999:7:::\npsios:" + operator + b":0:0:99999:7:::\n")
    (seed / "etc/shadow").chmod(0o600)
    (seed / "etc/os-release").write_text('NAME=PsiOS\nID=psios\nPRETTY_NAME="PsiOS amd64 development image"\n')
    (seed / "etc/securetty").write_text("ttyS0\n")
    (seed / "etc/issue").write_text("PsiOS amd64 development console\n")
    (seed / "etc/psios/core.json").write_text(json.dumps({
        "state_dir": "/var/lib/psios", "witness_dir": "/var/lib/psios.witness", "socket": "/run/psios/api.sock"}))
    (seed / "etc/inittab").write_text("::respawn:/bin/su -s /bin/sh psios -c 'exec /bin/psiosd serve'\n"
                                      "ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100\n"
                                      "::shutdown:/bin/sh /psios-shutdown\n")
    # Target permissions are independent of the builder's umask.
    for path in (seed / "etc").rglob("*"):
        path.chmod(0o755 if path.is_dir() else (0o600 if path.name == "shadow" else 0o644))
    (seed / "etc").chmod(0o755)


def ramfs(work, args, instance, home_id):
    boot_media.prepare_kernel(work, args.kernel_iso)
    (work / "bin").mkdir()
    install_core(work, args.core_archive, args.core_sha256)
    (work / "psios-disk-uuid").write_text(str(instance) + "\n")
    (work / "psios-home-uuid").write_text(str(home_id) + "\n")
    shutil.copyfile(HERE / "init", work / "psios-init")
    shutil.copyfile(HERE / "shutdown", work / "psios-shutdown")
    files = ["bin/psiosd", "bin/psiosctl", "psios-source", "psios-init", "psios-disk-uuid", "psios-home-uuid", "psios-shutdown"]
    for name in files:
        (work / name).chmod(0o755 if name.startswith("bin/") or name == "psios-init" else 0o644)
    with (work / "initrd").open("ab") as out:
        out.write(b"\0" * (-out.tell() % 4))
        out.flush()
        subprocess.run(["cpio", "--quiet", "-o", "-H", "newc", "--reproducible", "--owner=0:0"], cwd=work,
                       input=("\n".join(files) + "\n").encode(), stdout=out, check=True)


def seeded_ext4(work, name, label, identity, size_mib, seed, owners):
    image = work / name
    with image.open("xb") as f:
        f.truncate(size_mib * MIB)
    boot_media.run(work, ["mkfs.ext4", "-q", "-F", "-U", str(identity), "-L", label,
                         "-E", "root_owner=0:0", "-d", seed, image])
    # mke2fs -d preserves builder credentials. Set target UID/GID explicitly.
    fields = []
    for path in sorted(seed.rglob("*")):
        relative = path.relative_to(seed).as_posix()
        uid = owners.get(relative, 0)
        fields.extend(f"set_inode_field /{relative} {field} {uid}\n" for field in ("uid", "gid"))
    subprocess.run(["debugfs", "-w", "-f", "-", image], input="".join(fields), text=True,
                   stdout=subprocess.DEVNULL, check=True, timeout=20)
    boot_media.run(work, ["e2fsck", "-f", "-n", image])
    return image


def build(args):
    output = args.output.absolute()
    if os.path.lexists(output):
        raise ValueError("output already exists")
    with tempfile.TemporaryDirectory(dir=output.parent, prefix=".psios-disk-") as tmp:
        work = Path(tmp)
        instance = uuid.uuid4()
        home_id = uuid.uuid4()
        ramfs(work, args, instance, home_id)
        source_commit = (work / "psios-source").read_text().strip()
        efi = boot_media.efi_application(work, CMDLINE)
        esp = work / "esp.fat"
        with esp.open("xb") as f:
            f.truncate(64 * MIB)
        boot_media.run(work, ["mkfs.fat", "-F", "32", "--invariant", "-n", "PSIOSBOOT", esp])
        boot_media.run(work, ["mmd", "-i", esp, "::/EFI", "::/EFI/BOOT"])
        boot_media.run(work, ["mcopy", "-i", esp, efi, "::/EFI/BOOT/BOOTX64.EFI"])
        seed = work / "seed"
        seed.mkdir()
        seed.chmod(0o755)
        (seed / "initialize-once").touch()
        (seed / "deployment-uuid").write_text(str(instance) + "\n")
        for name in ("initialize-once", "deployment-uuid"):
            (seed / name).chmod(0o444)
        host_config(seed, args)
        state = seeded_ext4(work, "state.ext4", "PSIOSSTATE", instance, 192, seed, {})
        homes = work / "home-seed"
        homes.mkdir()
        homes.chmod(0o755)
        for name in ("root", "psios"):
            (homes / name).mkdir()
            (homes / name).chmod(0o700)
        home = seeded_ext4(work, "home.ext4", "PSIOSHOME", home_id, 64, homes, {"psios": 1000})
        disk = work / "installed.raw"
        with disk.open("xb") as f:
            f.truncate(323 * MIB)
        disk.chmod(0o600)
        layout = "label: gpt\nunit: sectors\nstart=2048,size=131072,type=U\nstart=133120,size=393216,type=L\nstart=526336,size=131072,type=L\n"
        subprocess.run(["sfdisk", "--quiet", disk], input=layout, text=True, check=True, timeout=30)
        boot_media.run(work, ["sfdisk", "--verify", disk])
        with disk.open("r+b") as f:
            for start, partition in [(1, esp), (65, state), (257, home)]:
                f.seek(start * MIB)
                with partition.open("rb") as src:
                    shutil.copyfileobj(src, f)
            f.flush()
            os.fsync(f.fileno())
        # No existing image or block device is ever an install target.
        os.link(disk, output)
        fd = os.open(output.parent, os.O_DIRECTORY | os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    print(json.dumps({"state_uuid": str(instance), "home_uuid": str(home_id), "source_commit": source_commit, "image": str(output)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--kernel-iso", required=True, type=Path)
    parser.add_argument("--core-archive", required=True, type=Path)
    parser.add_argument("--core-sha256", required=True, help="complete archive digest obtained after provenance verification")
    parser.add_argument("--console-hash", required=True, type=Path, help="private file containing an operator SHA-512 crypt hash")
    parser.add_argument("--root-console-hash", type=Path, help="optional private hash for local system administration; default locks root")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{64}", args.core_sha256):
        parser.error("invalid Core archive SHA-256")
    build(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError, tarfile.TarError) as error:
        sys.exit(str(error))
