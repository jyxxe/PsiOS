#!/usr/bin/env python3
"""Build a new PsiOS UEFI disk or prepare an offline upgrade of a stopped node."""
import argparse
from functools import partial
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import struct
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
DISK_SIZE = 323 * MIB
PARTITIONS = (
    (1, 64, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
    (65, 192, "0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
    (257, 64, "0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
)
EFI_PATH = "::/EFI/BOOT/BOOTX64.EFI"


def sha256(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


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


def unicast_mac(value):
    value = value.lower()
    if not re.fullmatch(r"[0-9a-f][02468ace](?::[0-9a-f]{2}){5}", value) or value == "00:00:00:00:00:00":
        raise argparse.ArgumentTypeError("expected a nonzero unicast MAC address")
    return value


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
    inittab = ("::respawn:/bin/su -s /bin/sh psios -c 'exec /bin/psiosd serve'\n"
               "ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100\n"
               "::shutdown:/bin/sh /psios-shutdown\n")
    if args.dhcp_mac:
        (seed / "etc/psios/dhcp-mac").write_text(args.dhcp_mac + "\n")
        (seed / "etc/resolv.conf").symlink_to("/run/psios-network/resolv.conf")
        inittab += "::respawn:/bin/sh /psios-dhcp\n"
    (seed / "etc/inittab").write_text(inittab)
    # Target permissions are independent of the builder's umask.
    for path in (seed / "etc").rglob("*"):
        if not path.is_symlink():
            path.chmod(0o755 if path.is_dir() else (0o600 if path.name in ("shadow", "dhcp-mac") else 0o644))
    (seed / "etc").chmod(0o755)


def ramfs(work, args, instance, home_id):
    base = boot_media.prepare_kernel(work, args.kernel_iso)
    (work / "bin").mkdir()
    install_core(work, args.core_archive, args.core_sha256)
    (work / "psios-disk-uuid").write_text(str(instance) + "\n")
    (work / "psios-home-uuid").write_text(str(home_id) + "\n")
    boot_scripts = ["init", "shutdown", "dhcp", "dhcp-event"]
    for script in boot_scripts:
        shutil.copyfile(HERE / script, work / f"psios-{script}")
    files = ["bin/psiosd", "bin/psiosctl", "psios-source", "psios-disk-uuid", "psios-home-uuid"]
    files.extend(f"psios-{script}" for script in boot_scripts)
    payload = {"/" + name: sha256(work / name) for name in files}
    inputs = {
        "format": 1, "architecture": "x86_64", "firmware": "UEFI",
        "core_source_commit": (work / "psios-source").read_text().strip(), "core_archive_sha256": args.core_sha256,
        "kernel_input": {name: base[name] for name in ("release", "sha256", "kernel_sha256", "initramfs_sha256")},
        "required_filesystems": {"state": str(instance), "home": str(home_id)},
        "payload_sha256": payload,
    }
    manifest = work / "psios-deployment.json"
    manifest.write_text(json.dumps(inputs, sort_keys=True, separators=(",", ":")) + "\n")
    # BusyBox can verify the exact extracted bytes without a JSON interpreter.
    checks = {**payload, "/psios-deployment.json": sha256(manifest)}
    (work / "psios-payload.sha256").write_text("".join(f"{digest}  {name}\n" for name, digest in checks.items()))
    files.extend(["psios-deployment.json", "psios-payload.sha256"])
    for name in files:
        (work / name).chmod(0o755 if name.startswith("bin/") or name in ("psios-init", "psios-dhcp-event") else 0o644)
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


def new_disk(work, disk, args, instance, home_id):
    seed = work / "seed"
    seed.mkdir()
    seed.chmod(0o755)
    (seed / "initialize-once").touch()
    (seed / "deployment-uuid").write_text(str(instance) + "\n")
    for name in ("initialize-once", "deployment-uuid"):
        (seed / name).chmod(0o444)
    host_config(seed, args)
    state = seeded_ext4(work, "state.ext4", "PSIOSSTATE", instance, PARTITIONS[1][1], seed, {})
    homes = work / "home-seed"
    homes.mkdir()
    homes.chmod(0o755)
    for name in ("root", "psios"):
        (homes / name).mkdir()
        (homes / name).chmod(0o700)
    home = seeded_ext4(work, "home.ext4", "PSIOSHOME", home_id, PARTITIONS[2][1], homes, {"psios": 1000})
    with disk.open("xb") as f:
        f.truncate(DISK_SIZE)
    layout = "label: gpt\nunit: sectors\n" + "".join(
        f"start={start * 2048},size={size * 2048},type={kind}\n" for start, size, kind in PARTITIONS)
    subprocess.run(["sfdisk", "--quiet", disk], input=layout, text=True, check=True, timeout=30)
    boot_media.run(work, ["sfdisk", "--verify", disk])
    for index, partition in ((1, state), (2, home)):
        write_partition(disk, index, partition)


def write_partition(disk, index, source):
    start, size, _ = PARTITIONS[index]
    if source.stat().st_size != size * MIB:
        raise ValueError("partition image has unexpected size")
    with disk.open("r+b") as dst, source.open("rb") as src:
        dst.seek(start * MIB)
        shutil.copyfileobj(src, dst)


def read_receipt(path):
    with boot_media.regular_file(path) as receipt:
        raw = receipt.read(16 * 1024 + 1)
    if len(raw) > 16 * 1024:
        raise ValueError("previous build receipt exceeds size limit")
    prior = json.loads(raw)
    if not isinstance(prior, dict) or type(prior.get("format")) is not int or prior["format"] != 1:
        raise ValueError("unsupported previous build receipt format")
    for name, pattern in (("source_commit", r"[0-9a-f]{40}"), ("efi_application_sha256", r"[0-9a-f]{64}")):
        if not isinstance(prior.get(name), str) or not re.fullmatch(pattern, prior[name]):
            raise ValueError(f"invalid previous build receipt field {name}")
    for name in ("state_uuid", "home_uuid"):
        value = prior.get(name)
        if not isinstance(value, str) or str(uuid.UUID(value)) != value:
            raise ValueError(f"invalid previous build receipt field {name}")
    if prior["state_uuid"] == prior["home_uuid"]:
        raise ValueError("previous deployment filesystems have identical UUIDs")
    return prior


def validate_layout(work, disk):
    boot_media.run(work, ["sfdisk", "--verify", disk])
    gpt = json.loads(subprocess.check_output(["sfdisk", "--json", disk], timeout=30))["partitiontable"]
    expected = [(start * 2048, size * 2048, kind) for start, size, kind in PARTITIONS]
    if (gpt["label"], gpt["unit"], gpt["sectorsize"]) != ("gpt", "sectors", 512) or [
            (p["start"], p["size"], p["type"]) for p in gpt["partitions"]] != expected:
        raise ValueError("unsupported previous PsiOS GPT layout")


def file_limit(max_bytes):
    soft, _ = resource.getrlimit(resource.RLIMIT_FSIZE)
    limit = max_bytes if soft == resource.RLIM_INFINITY else min(max_bytes, soft)
    return partial(resource.setrlimit, resource.RLIMIT_FSIZE, (limit, limit))


def filesystem_state(path, expected_uuid):
    with path.open("rb") as fs:
        fs.seek(1024)
        superblock = fs.read(1024)
    magic, state = struct.unpack_from("<HH", superblock, 56)
    if magic != 0xEF53 or str(uuid.UUID(bytes=superblock[104:120])) != expected_uuid:
        raise ValueError("ext4 partition does not match the previous deployment UUID")
    compat, incompat, ro_compat = struct.unpack_from("<III", superblock, 92)
    journal_inode, journal_device = struct.unpack_from("<II", superblock, 224)
    if (compat & 4) == 0 or journal_inode == 0 or journal_device != 0 or any(superblock[208:224]):
        raise ValueError("expected an ext4 filesystem with an internal journal")
    # These are EXT4_VALID_FS, HAS_JOURNAL, RECOVER and ORPHAN_PRESENT.
    return state, bool(incompat & 4 or ro_compat & 0x10000)


def check_filesystem(work, disk, index, expected_uuid):
    start, size, _ = PARTITIONS[index]
    fs = work / f"previous-partition-{index + 1}.ext4"
    with disk.open("rb") as source, fs.open("xb") as out:
        source.seek(start * MIB)
        for _ in range(size):
            chunk = source.read(MIB)
            if len(chunk) != MIB:
                raise ValueError("incomplete filesystem in previous disk")
            out.write(chunk)
    state, pending = filesystem_state(fs, expected_uuid)
    if pending and (state & ~1) == 0:
        replay = subprocess.run(["e2fsck", "-p", "-E", "journal_only", fs], stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, timeout=90, preexec_fn=file_limit(size * MIB))
        if replay.returncode not in (0, 1):
            raise ValueError(f"could not replay partition {index + 1} ext4 journal")
        state, still_pending = filesystem_state(fs, expected_uuid)
    else:
        still_pending = pending
    if state != 1 or still_pending:
        raise ValueError(f"partition {index + 1} requires filesystem repair before upgrade")
    boot_media.run(work, ["e2fsck", "-f", "-n", fs])
    if pending:
        write_partition(disk, index, fs)
    return fs, pending


def read_partition_file(work, fs, member, limit):
    name = Path(member).name
    subprocess.run(["debugfs", "-R", f"dump {member} {name}", fs], cwd=work,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True,
                   timeout=20, preexec_fn=file_limit(limit + 1))
    with boot_media.regular_file(work / name) as f:
        data = f.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"previous disk file exceeds size limit: {member}")
    return data


def source_snapshot(work, disk, args, prior):
    with boot_media.regular_file(args.upgrade_from) as source:
        fd = source.fileno()
        before = os.fstat(fd)
        if before.st_size != DISK_SIZE:
            raise ValueError("unsupported previous PsiOS disk size")
        options = f"driver=raw,file.driver=file,file.filename=/proc/self/fd/{fd},file.locking=on"
        subprocess.run(["qemu-img", "convert", "--image-opts", "-m", "1", "-O", "raw", options, disk],
                       pass_fds=(fd,), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, check=True,
                       timeout=90, preexec_fn=file_limit(DISK_SIZE))
        after = os.fstat(fd)
        if (after.st_size, after.st_mtime_ns, after.st_ctime_ns) != (before.st_size, before.st_mtime_ns, before.st_ctime_ns):
            raise ValueError("previous disk changed during capture")
    if disk.stat().st_size != DISK_SIZE:
        raise ValueError("incomplete previous disk capture")
    digest = sha256(disk)
    validate_layout(work, disk)
    old_efi = work / "previous.efi"
    boot_media.run(work, ["mcopy", "-i", f"{disk.name}@@{MIB}", EFI_PATH, old_efi])
    if sha256(old_efi) != prior["efi_application_sha256"]:
        raise ValueError("previous EFI application does not match the trusted build receipt")
    state, replayed_state = check_filesystem(work, disk, 1, prior["state_uuid"])
    homes, replayed_home = check_filesystem(work, disk, 2, prior["home_uuid"])
    homes.unlink()
    marker = read_partition_file(work, state, "/deployment-uuid", 64)
    if marker != (prior["state_uuid"] + "\n").encode():
        raise ValueError("state partition deployment marker does not match the prior receipt")
    cfg = json.loads(read_partition_file(work, state, "/etc/psios/core.json", 64 * 1024))
    state.unlink()
    if not isinstance(cfg, dict) or (cfg.get("state_dir"), cfg.get("witness_dir")) != ("/var/lib/psios", "/var/lib/psios.witness"):
        raise ValueError("offline upgrade requires the native state and witness locations")
    replayed = [part for part, replay in ((2, replayed_state), (3, replayed_home)) if replay]
    return digest, replayed


def verify_authority(work, disk):
    core = work / "bin" / "psiosd"
    return json.loads(subprocess.check_output([
        sys.executable, HERE.parent / "rescue" / "inspect.py", disk,
        "--iso", work / "base.iso", "--core", core, "--core-sha256", sha256(core),
        "--partition", "2", "--state", "/psios", "--uid", "1000",
    ], timeout=210))


def build(args):
    output = args.output.absolute()
    if os.path.lexists(output):
        raise ValueError("output already exists")
    with tempfile.TemporaryDirectory(dir=output.parent, prefix=".psios-disk-") as tmp:
        work = Path(tmp)
        disk = work / "installed.raw"
        prior = read_receipt(args.previous_receipt) if args.upgrade_from else None
        if prior:
            instance, home_id = prior["state_uuid"], prior["home_uuid"]
            source_digest, replayed = source_snapshot(work, disk, args, prior)
        else:
            instance, home_id = str(uuid.uuid4()), str(uuid.uuid4())
            new_disk(work, disk, args, instance, home_id)
        disk.chmod(0o600)
        ramfs(work, args, instance, home_id)
        source_commit = (work / "psios-source").read_text().strip()
        authority = verify_authority(work, disk) if prior else None
        efi = boot_media.efi_application(work, CMDLINE)
        esp = work / "esp.fat"
        with esp.open("xb") as f:
            f.truncate(PARTITIONS[0][1] * MIB)
        boot_media.run(work, ["mkfs.fat", "-F", "32", "--invariant", "-n", "PSIOSBOOT", esp])
        boot_media.run(work, ["mmd", "-i", esp, "::/EFI", "::/EFI/BOOT"])
        boot_media.run(work, ["mcopy", "-i", esp, efi, EFI_PATH])
        write_partition(disk, 0, esp)
        with disk.open("rb") as completed:
            os.fsync(completed.fileno())
        receipt = {
            "format": 1, "image": str(output), "source_commit": source_commit,
            "state_uuid": instance, "home_uuid": home_id,
            "deployment_manifest_sha256": sha256(work / "psios-deployment.json"),
            "efi_application_sha256": sha256(efi), "installed_initramfs_sha256": sha256(work / "initrd"),
            "initial_image_sha256": sha256(disk),
        }
        if prior:
            receipt["upgrade"] = {
                "previous_image_sha256": source_digest,
                "previous_source_commit": prior["source_commit"],
                "verified_authority": authority,
                "replayed_ext4_partitions": replayed,
            }
        # No existing image or block device is ever an install target.
        os.link(disk, output)
        fd = os.open(output.parent, os.O_DIRECTORY | os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    print(json.dumps(receipt))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--kernel-iso", required=True, type=Path)
    parser.add_argument("--core-archive", required=True, type=Path)
    parser.add_argument("--core-sha256", required=True, help="complete archive digest obtained after provenance verification")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--console-hash", type=Path, help="private file containing an operator SHA-512 crypt hash")
    mode.add_argument("--upgrade-from", type=Path, help="stopped native development disk to copy without changes")
    parser.add_argument("--previous-receipt", type=Path, help="trusted build receipt for the stopped disk")
    parser.add_argument("--root-console-hash", type=Path, help="optional private hash for local system administration; default locks root")
    parser.add_argument("--dhcp-mac", type=unicast_mac, help="enable IPv4 DHCP on the matching virtio NIC of a new disk")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{64}", args.core_sha256):
        parser.error("invalid Core archive SHA-256")
    if args.upgrade_from and (not args.previous_receipt or args.root_console_hash or args.dhcp_mac):
        parser.error("upgrade requires --previous-receipt and preserves console credentials and network configuration")
    if args.console_hash and args.previous_receipt:
        parser.error("--previous-receipt requires --upgrade-from")
    build(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, tarfile.TarError) as error:
        sys.exit(str(error))
