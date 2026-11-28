#!/usr/bin/env python3
"""Install to a disposable Virtio, NVMe or SATA AHCI disk and boot the installed PsiOS."""

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

from native_disk import REPO, boot as boot_native, check_clean_shutdown, check_state, password, sha
from native_install import sparse
from qemu_boot import boot as boot_ubuntu, identity


TARGET_SERIAL = "PSIOS-INSTALL-001"
BAD_SERIAL = {"virtio": "PSIOS BAD SERIAL", "nvme": "  ", "ahci": "PSIOS-NO-WWN"}
COLLISION_SERIAL = "PSIOS-COLLISION-"
GUEST = Path("/var/lib/psios-virtio-acceptance")
OPERATION = GUEST / "operation"
BAD_OPERATION = GUEST / "bad-operation"
INSTALLER = GUEST / "native/install.py"
MIB = 1024 * 1024
TOOLS = {
    "native/core-cgroup", "native/create.py", "native/dhcp", "native/dhcp-event",
    "native/enroll", "native/init", "native/install.py", "native/install_source.py",
    "native/install_target.py", "native/ipv4", "native/ipv6", "native/shutdown",
    "native/static", "native/storage", "rescue/base.json", "rescue/boot_media.py",
}

# Read kernel evidence independently of the installer. Select by transport
# identity before comparing the installer plan with the kernel observations.
PROBE_DEVICE = """
import json, os, stat, sys
from pathlib import Path

def read(path):
    return path.read_text(encoding="ascii").removesuffix("\\n")

bus, serial = sys.argv[1:]
if bus == "nvme":
    serial = serial.strip(" ")
    disk, = (p.resolve(strict=True) for p in Path("/sys/block").glob("nvme*n*")
             if read(p.resolve(strict=True).parent / "serial").strip(" ") == serial)
    controller = disk.parent
    assert read(controller / "transport") == "pcie"
    assert (controller.parent.parent / "driver").resolve() == Path("/sys/bus/pci/drivers/nvme")
    transport = dict(kind="NVMeNamespace", serial=serial, namespace_id=int(read(disk / "nsid")),
                     wwid=read(disk / "wwid"), sysfs_path=str(controller))
elif bus == "ahci":
    disk, = (p.resolve(strict=True) for p in Path("/sys/block").glob("sd*")
             if (p / "device/vpd_pg80").is_file()
             and (p / "device/vpd_pg80").read_bytes()[4:].decode("ascii").strip(" ") == serial)
    device = disk.parent.parent
    assert (device / "driver").resolve() == Path("/sys/bus/scsi/drivers/sd")
    controller = device.parent.parent.parent.parent
    assert read(controller / "class") == "0x010601"
    assert (controller / "driver").resolve() == Path("/sys/bus/pci/drivers/ahci")
    transport = dict(kind="SataAhciDisk", serial=serial, wwid=read(device / "wwid"), sysfs_path=str(device))
else:
    disk, = (p.resolve(strict=True) for p in Path("/sys/block").glob("vd*") if read(p / "serial") == serial)
    assert (disk.parent.parent / "driver").resolve() == Path("/sys/bus/virtio/drivers/virtio_blk")
    transport = dict(kind="VirtioBlock", serial=serial, sysfs_path=str(disk.parent.parent))
device = Path("/dev") / disk.name
info = device.lstat()
assert stat.S_ISBLK(info.st_mode), info
major_minor = f"{os.major(info.st_rdev)}:{os.minor(info.st_rdev)}"
assert major_minor == read(disk / "dev"), major_minor
print(json.dumps(dict(
    path=str(device), major_minor=major_minor, sysfs_path=str(disk),
    boot_id=read(Path("/proc/sys/kernel/random/boot_id")),
    disk_sequence=int(read(disk / "diskseq")), size_bytes=int(read(disk / "size")) * 512,
    logical_block_bytes=int(read(disk / "queue/logical_block_size")),
    physical_block_bytes=int(read(disk / "queue/physical_block_size")),
    read_only=bool(int(read(disk / "ro"))),
    transport=transport,
)))
"""


def remote(run, *args, **options):
    return run(shlex.join(map(str, args)), **options)


def upload(run, source, destination, *, sparse_image=False):
    expected = sha(source)
    receive = ('umask 077; dd of="$1" bs=1M iflag=fullblock conv=sparse status=none' if sparse_image
               else 'umask 077; cat > "$1"')
    with source.open("rb") as data:
        remote(run, "sudo", "-n", "sh", "-c", receive, "sh", destination, stdin=data, timeout=180)
    readback = remote(run, "sudo", "-n", "sha256sum", "--", destination).stdout.split()[0]
    assert readback == expected, f"streamed input differs in Ubuntu: {destination.name}"
    return expected


def extract_tools(archive, work):
    pending = TOOLS | {"SHA256SUMS"}
    with tarfile.open(archive, "r|gz") as bundle:
        for member in bundle:
            if member.name not in TOOLS and member.name != "SHA256SUMS":
                continue
            assert member.name in pending and member.isfile() and 0 < member.size <= MIB, member.name
            path = work / member.name
            with bundle.extractfile(member) as source, path.open("xb") as destination:
                shutil.copyfileobj(source, destination)
            pending.remove(member.name)
    assert not pending, f"portable archive lacks native installer tools: {sorted(pending)}"
    sums = {}
    for line in (work / "SHA256SUMS").read_text().splitlines():
        expected, name = line.split(maxsplit=1)
        sums[name] = expected
    for name in TOOLS:
        assert sha(work / name) == sums[name], f"portable archive tool digest differs: {name}"


def make_source(args, work):
    for directory in ("native", "rescue"):
        (work / directory).mkdir()
    extract_tools(args.archive, work)
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "admin.hash")
    image = work / "factory.raw"
    receipt = work / "factory.json"
    with receipt.open("xb") as output:
        subprocess.run([sys.executable, "-B", work / "native/create.py", image,
                        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive,
                        "--core-sha256", args.archive_sha256, "--console-hash", operator_hash,
                        "--root-console-hash", admin_hash], check=True, stdout=output, timeout=240)
    return image, receipt, operator, admin


def ubuntu_command(instance, target, rejected, port, serial_log, bus="virtio", collisions=()):
    accel = "kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    manifest = json.loads((instance / "manifest.json").read_bytes())
    assert manifest["firmware"] == "OVMF" and manifest["architecture"] == "amd64"
    assert sha(instance / "uefi-code.fd") == manifest["ovmf"]["code_sha256"]
    assert sha(instance / "uefi-vars.fd") == manifest["ovmf"]["initial_vars_sha256"]
    cmd = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot",
           "-machine", "q35", "-accel", accel, "-cpu", "host" if accel == "kvm" else "max",
           "-smp", "1", "-m", "1024", "-display", "none", "-monitor", "none",
           "-serial", f"file:{serial_log.name}", "-boot", "strict=on",
           "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=uefi-code.fd",
           "-drive", "if=pflash,format=raw,unit=1,file=uefi-vars.fd",
           "-netdev", f"user,id=management,restrict=on,hostfwd=tcp:127.0.0.1:{port}-:22",
           "-device", "virtio-net-pci,netdev=management,romfile="]
    for name, path, fmt, serial, options in (
        ("ubuntu", "system.qcow2", "qcow2", "PSIOS-UBUNTU", ",bootindex=1"),
        ("seed", "seed.iso", "raw", "PSIOS-SEED", ""),
        ("installed", f"../{target.name}", "raw", TARGET_SERIAL, ""),
        ("rejected", f"../{rejected.name}", "raw", BAD_SERIAL[bus], ""),
    ):
        read_only = ",readonly=on" if name == "seed" else ""
        device = "virtio-blk-pci"
        if name in ("installed", "rejected"):
            if bus == "nvme":
                device = "nvme"
            elif bus == "ahci":
                port = 0 if name == "installed" else 1
                device = f"ide-hd,bus=ide.{port}"
                if name == "installed":
                    options += ",wwn=0x5000c500f0120001"
        cmd += ["-drive", f"if=none,id={name},format={fmt},file={path}{read_only}",
                "-device", f"{device},drive={name},serial={serial}{options}"]
    for index, collision in enumerate(collisions):
        cmd += ["-drive", f"if=none,id=collision{index},format=raw,file=../{collision.name}",
                "-device", f"ide-hd,bus=ide.{index + 2},drive=collision{index},"
                           f"serial={COLLISION_SERIAL}{index},wwn=0x5000c500f0120002"]
    return cmd


def install_in_guest(run, tool_debs, work, source, receipt, source_digest, blank_digest, bus):
    remote(run, "sudo", "-n", "install", "-d", "-m", "0700",
           GUEST, GUEST / "native", GUEST / "rescue", OPERATION, BAD_OPERATION)
    packages = []
    for index, package in enumerate(tool_debs):
        path = GUEST / f"tool-{index}.deb"
        upload(run, package, path)
        packages.append(path)
    if packages:
        remote(run, "sudo", "-n", "dpkg", "-i", *packages, timeout=120)
    remote(run, "sudo", "-n", "sh", "-c", "for tool in python3 sfdisk mcopy e2fsck debugfs blkid; do "
           "command -v \"$tool\" >/dev/null || { echo \"missing offline installer tool: $tool\" >&2; exit 1; }; done")
    for path in sorted(TOOLS):
        upload(run, work / path, GUEST / path)
    guest_image, guest_receipt = GUEST / source.name, GUEST / receipt.name
    uploaded_image = upload(run, source, guest_image, sparse_image=True)
    assert uploaded_image == source_digest
    receipt_digest = upload(run, receipt, guest_receipt)

    def install(operation, verb, *args, **options):
        return remote(run, "sudo", "-n", "python3", "-B", INSTALLER, verb, "--state-dir", operation,
                      *args, timeout=180, **options)

    evidence = json.loads(remote(run, "python3", "-c", PROBE_DEVICE, bus, TARGET_SERIAL).stdout)
    bad_evidence = json.loads(remote(run, "python3", "-c", PROBE_DEVICE, bus, BAD_SERIAL[bus]).stdout)
    assert evidence["path"] != bad_evidence["path"]
    assert evidence["size_bytes"] == bad_evidence["size_bytes"] == source.stat().st_size
    assert evidence["disk_sequence"] > 0 and evidence["disk_sequence"] != bad_evidence["disk_sequence"]
    assert evidence["logical_block_bytes"] == 512 and not evidence["read_only"]
    plan_options = ("--source-image", guest_image, "--receipt", guest_receipt,
                    "--receipt-sha256", receipt_digest, "--target")
    bad = install(BAD_OPERATION, "plan", *plan_options, bad_evidence["path"], check=False)
    bad_identity = "registered NAA WWID" if bus == "ahci" else "stable printable serial"
    assert bad.returncode == 1 and bad_identity in bad.stderr, bad.stderr
    remote(run, "sudo", "-n", "test", "!", "-e", BAD_OPERATION / "operation.json")
    if bus == "ahci":
        collision = json.loads(remote(run, "python3", "-c", PROBE_DEVICE, bus, COLLISION_SERIAL + "0").stdout)
        duplicate = install(BAD_OPERATION, "plan", *plan_options, collision["path"], check=False)
        assert duplicate.returncode == 1 and "SATA installation disk identity is not unique" in duplicate.stderr, duplicate.stderr
        remote(run, "sudo", "-n", "test", "!", "-e", BAD_OPERATION / "operation.json")

    planned = json.loads(install(OPERATION, "plan", *plan_options, evidence["path"]).stdout)
    assert planned["status"] == "Planned" and planned["plan"]["allow_loop"] is False, planned
    assert planned["plan"]["target"] == evidence, planned
    assert planned["plan"]["blank_sha256"] == blank_digest, planned
    assert planned["plan"]["source"]["image_sha256"] == source_digest, planned
    assert planned["plan"]["source"]["receipt_sha256"] == receipt_digest, planned
    refused = install(OPERATION, "apply", "--plan-digest", "0" * 64, check=False)
    assert refused.returncode == 1 and "approval does not match" in refused.stderr, refused.stderr
    assert json.loads(install(OPERATION, "inspect").stdout) == planned
    before = remote(run, "sudo", "-n", "sha256sum", "--", evidence["path"], timeout=90).stdout
    assert before.split()[0] == blank_digest, "wrong approval wrote to the blank target"

    verified = json.loads(install(OPERATION, "apply", "--plan-digest", planned["plan_digest"]).stdout)
    assert verified["status"] == "Verified" and verified["plan"] == planned["plan"], verified
    assert verified["plan_digest"] == planned["plan_digest"], verified
    assert verified["image_readback_sha256"] == source_digest, verified
    assert json.loads(install(OPERATION, "inspect").stdout) == verified
    assert json.loads(install(OPERATION, "apply", "--plan-digest", planned["plan_digest"]).stdout) == verified
    return evidence


def boot_installed(args, work, target, receipt, operator, admin):
    deployment = json.loads(receipt.read_bytes())
    for source, target_name in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / target_name)

    def sample():
        with boot_native(work, target, vcpus=1, disk_bus=args.target_bus, pace_commands=True) as console:
            console.login("psios", operator)
            assert console.run("sha256sum /psios-deployment.json").split()[0] == deployment["deployment_manifest_sha256"]
            state = check_state(console)
            assert state[2]["availability"] == "Available", state[2]
            console.send("exit")
            console.login("root", admin)
            console.send("cd /; exec poweroff")
            assert console.vm.wait(60) == 0, "installed guest did not shut down cleanly"
        check_clean_shutdown(target)
        return state

    authority, first, result, boot = sample()
    same_authority, replay, same_result, next_boot = sample()
    assert authority == same_authority and boot != next_boot
    assert first["task_id"] == replay["task_id"] and replay["deduplicated"] and result == same_result
    return authority, first["task_id"]


def exercise(args, work):
    key = work / "admin"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key],
                   check=True, timeout=15)
    instance = work / "ubuntu"
    subprocess.run([sys.executable, "-B", REPO / "images/qemu/create.py", "--base", args.base,
                    "--package", args.package, "--authorized-key", key.with_suffix(".pub"),
                    "--output", instance, "--firmware", "uefi", "--ovmf-dir", args.ovmf_dir],
                   check=True, timeout=80)
    manifest = json.loads((instance / "manifest.json").read_bytes())
    assert manifest["package_sha256"] == sha(args.package)
    image, receipt, operator, admin = make_source(args, work)
    source_digest = sha(image)
    assert source_digest == json.loads(receipt.read_bytes())["initial_image_sha256"]
    target = sparse(work, "installed.raw", image.stat().st_size)
    rejected = sparse(work, "rejected.raw", image.stat().st_size)
    collisions = (tuple(sparse(work, f"collision-{index}.raw", image.stat().st_size) for index in range(2))
                  if args.target_bus == "ahci" else ())
    blank = sha(target)
    assert sha(rejected) == blank

    def launch(port, serial_log):
        return ubuntu_command(instance, target, rejected, port, serial_log, args.target_bus, collisions)

    with boot_ubuntu(instance, key, 1, launch=launch, readiness_timeout=600) as run:
        print("Ubuntu installer host provisioned from the verified Core package", flush=True)
        ubuntu = identity(run)
        run("test -d /sys/firmware/efi")
        package = remote(run, "dpkg-query", "-W", "-f=${Version}", "psios-core").stdout.strip()
        assert package == manifest["package_version"], package
        evidence = install_in_guest(run, args.tool_deb, work, image, receipt, source_digest, blank, args.target_bus)
        assert evidence["boot_id"] == ubuntu[2], evidence
    assert sha(image) == sha(target) == source_digest, "installed raw bytes differ after Ubuntu shutdown"
    assert sha(rejected) == blank, "refused block device changed"
    assert all(sha(disk) == blank for disk in collisions), "duplicate-identity disk changed"
    authority, task = boot_installed(args, work, target, receipt, operator, admin)
    assert authority != ubuntu[0], "the installed raw disk reused the Ubuntu authority"
    print(f"PASS {evidence['transport']['kind']} {TARGET_SERIAL}, full raw image {source_digest}, "
          f"Ubuntu kernel disk sequence {evidence['disk_sequence']}, native Task {task} survived reboot")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True, help="pinned Ubuntu minimal QCOW2")
    parser.add_argument("--package", type=Path, required=True, help="tested PsiOS Core amd64 DEB")
    parser.add_argument("--archive", type=Path, required=True, help="authenticated portable Core archive")
    parser.add_argument("--archive-sha256", required=True, help="expected digest of the portable Core archive")
    parser.add_argument("--kernel-iso", type=Path, required=True, help="pinned kernel ISO")
    parser.add_argument("--target-bus", choices=("virtio", "nvme", "ahci"), default="virtio")
    parser.add_argument("--tool-deb", type=Path, action="append", default=[],
                        help="trusted offline Ubuntu tool or dependency package, repeat as needed")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    for name in ("base", "package", "archive", "kernel_iso", "ovmf_dir"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    args.tool_deb = [path.resolve(strict=True) for path in args.tool_deb]
    if len(args.tool_deb) > 16 or any(not path.is_file() or path.suffix != ".deb"
                                     or not 0 < path.stat().st_size <= 32 * MIB for path in args.tool_deb):
        parser.error("expected at most 16 local tool DEBs of at most 32 MiB each")
    if not (re.fullmatch(r"[0-9a-f]{64}", args.archive_sha256) and args.archive.is_file()
            and sha(args.archive) == args.archive_sha256):
        parser.error("portable Core archive differs from its expected SHA-256")
    with tempfile.TemporaryDirectory(prefix=f"psios-native-install-{args.target_bus}-") as work:
        exercise(args, Path(work).resolve())


if __name__ == "__main__":
    main()
