#!/usr/bin/env python3
"""Install to a disposable Virtio, NVMe or SATA AHCI disk and boot the installed PsiOS."""

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tarfile
import tempfile
import time

from native_disk import REPO, boot as boot_native, check_clean_shutdown, check_state, password, sha
from native_install import sparse
from qemu_boot import boot as boot_ubuntu, identity


TARGET_SERIAL = "PSIOS-INSTALL-001"
REPLACEMENT_SERIAL = "PSIOS-REPLACE-002"
BAD_SERIAL = {"virtio": "PSIOS BAD SERIAL", "nvme": "  ", "ahci": "PSIOS-NO-WWN"}
COLLISION_SERIAL = "PSIOS-COLLISION-"
GUEST = Path("/var/lib/psios-virtio-acceptance")
OPERATION = GUEST / "operation"
BAD_OPERATION = GUEST / "bad-operation"
REPLACEMENT_OPERATION = GUEST / "replacement-operation"
INSTALLER = GUEST / "native/install.py"
MIB = 1024 * 1024
TOOLS = {
    "native/ca-certificates.LICENSE", "native/core-cgroup", "native/create.py", "native/dhcp", "native/dhcp-event",
    "native/enroll", "native/init", "native/install.py", "native/install_source.py",
    "native/install_target.py", "native/ipv4", "native/ipv6", "native/shutdown",
    "native/static", "native/storage", "rescue/base.json", "rescue/boot_media.py",
}
BOOT_ASSET_DIRS = ("native", "rescue")
BOOT_ASSET = re.compile(r"(?:native|rescue)/[A-Za-z0-9_][A-Za-z0-9_.-]*\Z")

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
    # Factory assembly needs its packaged sibling assets. Guest installation
    # uses the fixed tool set transferred below.
    extracted = set()
    with tarfile.open(archive, "r|gz") as bundle:
        for member in bundle:
            name = member.name
            if name in BOOT_ASSET_DIRS and member.isdir():
                continue
            if name != "SHA256SUMS" and name.split("/", 1)[0] not in BOOT_ASSET_DIRS:
                continue
            assert name == "SHA256SUMS" or BOOT_ASSET.fullmatch(name), name
            assert name not in extracted and member.isfile() and 0 < member.size <= MIB, name
            with bundle.extractfile(member) as source, (work / name).open("xb") as destination:
                shutil.copyfileobj(source, destination)
            extracted.add(name)
    missing = (TOOLS | {"SHA256SUMS"}) - extracted
    assert not missing, f"portable archive lacks native installer tools: {sorted(missing)}"
    sums = {}
    for line in (work / "SHA256SUMS").read_text().splitlines():
        expected, name = line.split(maxsplit=1)
        assert name not in sums, f"duplicate portable archive digest: {name}"
        sums[name] = expected
    assets = extracted - {"SHA256SUMS"}
    assert assets == {name for name in sums if BOOT_ASSET.fullmatch(name)}, "portable archive assets differ from checksums"
    for name in sorted(assets):
        assert sha(work / name) == sums[name], f"portable archive tool digest differs: {name}"


def make_source(args, work):
    for directory in BOOT_ASSET_DIRS:
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


def ubuntu_command(instance, target, rejected, port, serial_log, bus="virtio", collisions=(), replacement=None):
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
    if replacement is not None:
        assert bus == "nvme"
        cmd += ["-qmp", "unix:control.sock,server=on,wait=off",
                "-device", "pcie-root-port,id=install-port,bus=pcie.0,slot=1",
                "-drive", f"if=none,id=replacement,format=raw,file=../{replacement.name}"]
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
                if name == "installed" and replacement is not None:
                    options += ",id=planned-disk,bus=install-port"
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


def replace_nvme(run, control, original):
    # Bound QMP responses and wait for PCIe eject acknowledgement from the guest.
    with socket.socket(socket.AF_UNIX) as monitor:
        monitor.settimeout(15)
        monitor.connect(str(control))
        with monitor.makefile("rb") as incoming:
            def receive():
                line = incoming.readline(64 * 1024)
                assert line.endswith(b"\n"), "invalid QMP message"
                return json.loads(line)

            def execute(name, **arguments):
                message = {"execute": name, "arguments": arguments, "id": name}
                monitor.sendall(json.dumps(message).encode("ascii") + b"\r\n")

            assert "QMP" in receive()
            execute("qmp_capabilities")
            reply = receive()
            assert reply == {"return": {}, "id": "qmp_capabilities"}, reply
            execute("device_del", id="planned-disk")
            reply = receive()
            assert reply == {"return": {}, "id": "device_del"}, reply
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                message = receive()
                if message.get("event") == "DEVICE_DELETED" and message["data"].get("device") == "planned-disk":
                    break
            else:
                raise AssertionError("guest did not acknowledge removal of the planned disk")
            remote(run, "sudo", "-n", "udevadm", "settle", "--timeout=20")
            remote(run, "test", "!", "-e", original["path"])
            execute("device_add", driver="nvme", drive="replacement", serial=REPLACEMENT_SERIAL,
                    id="replacement-disk", bus="install-port")
            reply = receive()
            assert reply == {"return": {}, "id": "device_add"}, reply

    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        result = remote(run, "python3", "-c", PROBE_DEVICE, "nvme", REPLACEMENT_SERIAL, check=False)
        if result.returncode == 0:
            observed = json.loads(result.stdout)
            break
        time.sleep(1)
    else:
        raise AssertionError("the replacement NVMe did not appear in the guest")
    for field in ("path", "sysfs_path", "boot_id", "size_bytes", "logical_block_bytes"):
        assert original[field] == observed[field], (field, original, observed)
    assert original["disk_sequence"] != observed["disk_sequence"], (original, observed)
    assert original["transport"]["wwid"] != observed["transport"]["wwid"], (original, observed)
    return observed


def install_in_guest(run, tool_debs, work, source, receipt, source_digest, blank_digest, bus, target_size, control=None):
    remote(run, "sudo", "-n", "install", "-d", "-m", "0700",
           GUEST, GUEST / "native", GUEST / "rescue", OPERATION, BAD_OPERATION)
    if control is not None:
        remote(run, "sudo", "-n", "install", "-d", "-m", "0700", REPLACEMENT_OPERATION)
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
    assert evidence["size_bytes"] == bad_evidence["size_bytes"] == target_size
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

    larger = target_size > source.stat().st_size
    options = ("--allow-larger-target",) if larger else ()
    if larger:
        default = install(OPERATION, "plan", *plan_options, evidence["path"], check=False)
        assert default.returncode == 1 and "same exact size" in default.stderr, default.stderr
        remote(run, "sudo", "-n", "test", "!", "-e", OPERATION / "operation.json")
    planned = json.loads(install(OPERATION, "plan", *plan_options, evidence["path"], *options).stdout)
    assert planned["status"] == "Planned" and planned["plan"]["allow_loop"] is False, planned
    assert planned["plan"]["target"] == evidence, planned
    assert planned["plan"]["blank_sha256"] == blank_digest, planned
    assert planned["plan"]["source"]["image_sha256"] == source_digest, planned
    assert planned["plan"]["source"]["receipt_sha256"] == receipt_digest, planned
    expected_image = source_digest
    if larger:
        deployment = planned["plan"]["deployment_image"]
        assert deployment["size_bytes"] == target_size and deployment["backup_gpt_offset"] == target_size - 33 * 512
        expected_image = deployment["image_sha256"]
        assert expected_image != source_digest
    else:
        assert "deployment_image" not in planned["plan"]
    refused = install(OPERATION, "apply", "--plan-digest", "0" * 64, check=False)
    assert refused.returncode == 1 and "approval does not match" in refused.stderr, refused.stderr
    assert json.loads(install(OPERATION, "inspect").stdout) == planned
    before = remote(run, "sudo", "-n", "sha256sum", "--", evidence["path"], timeout=90).stdout
    assert before.split()[0] == blank_digest, "wrong approval wrote to the blank target"

    operation = OPERATION
    if control is not None:
        replacement = replace_nvme(run, control, evidence)
        stale = install(OPERATION, "apply", "--plan-digest", planned["plan_digest"], check=False)
        assert stale.returncode == 1 and "target disk identity or generation differs from plan" in stale.stderr, stale.stderr
        assert json.loads(install(OPERATION, "inspect").stdout) == planned
        after = remote(run, "sudo", "-n", "sha256sum", "--", replacement["path"], timeout=90).stdout
        assert after.split()[0] == blank_digest, "stale plan wrote to the replacement disk"
        print(f"PASS NVMe path {evidence['path']} reused, WWID {evidence['transport']['wwid']} → "
              f"{replacement['transport']['wwid']}, kernel sequence {evidence['disk_sequence']} → "
              f"{replacement['disk_sequence']}; stale plan refused with unchanged bytes {blank_digest}", flush=True)
        operation = REPLACEMENT_OPERATION
        evidence = replacement
        updated = json.loads(install(operation, "plan", *plan_options, evidence["path"], *options).stdout)
        assert updated["status"] == "Planned", updated
        assert updated["plan"]["id"] != planned["plan"]["id"], updated
        assert updated["plan"] == {**planned["plan"], "id": updated["plan"]["id"], "target": evidence}, updated
        assert updated["plan_digest"] != planned["plan_digest"], updated
        planned = updated

    verified = json.loads(install(operation, "apply", "--plan-digest", planned["plan_digest"]).stdout)
    assert verified["status"] == "Verified" and verified["plan"] == planned["plan"], verified
    assert verified["plan_digest"] == planned["plan_digest"], verified
    assert verified["image_readback_sha256"] == expected_image, verified
    assert json.loads(install(operation, "inspect").stdout) == verified
    assert json.loads(install(operation, "apply", "--plan-digest", planned["plan_digest"]).stdout) == verified
    return evidence, expected_image


def check_larger_disk(factory, installed):
    subprocess.run(["sfdisk", "--verify", installed], check=True, stdout=subprocess.DEVNULL, timeout=30)

    def table(path):
        return json.loads(subprocess.check_output(["sfdisk", "--json", path], timeout=30))["partitiontable"]

    before, after = table(factory), table(installed)
    assert before["id"] == after["id"] and before["firstlba"] == after["firstlba"]
    capacity = installed.stat().st_size
    assert after["lastlba"] == capacity // 512 - 34
    fields = ("start", "size", "type", "uuid")
    assert [[item[key] for key in fields] for item in before["partitions"]] == [
        [item[key] for key in fields] for item in after["partitions"]]
    with factory.open("rb") as original, installed.open("rb") as result:
        protective = result.read(512)
        assert struct.unpack_from("<I", protective, 458)[0] == capacity // 512 - 1
        result.seek(capacity - 512)
        assert result.read(8) == b"EFI PART"
        old_backup = (before["lastlba"] + 1) * 512
        result.seek(old_backup)
        gap = result.read((after["lastlba"] + 1) * 512 - old_backup)
        assert gap and not any(gap), "the unused space contains old GPT bytes"
        for partition in before["partitions"]:
            start = partition["start"] * 512
            original.seek(start)
            result.seek(start)
            for offset in range(0, partition["size"] * 512, MIB):
                length = min(MIB, partition["size"] * 512 - offset)
                left, right = original.read(length), result.read(length)
                assert len(left) == length and left == right, (start, offset)


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
    target_size = image.stat().st_size + (4 * MIB + 17 * 512 if args.larger_target else 0)
    target = sparse(work, "installed.raw", target_size)
    rejected = sparse(work, "rejected.raw", target_size)
    replacement = sparse(work, "replacement.raw", target_size) if args.target_bus == "nvme" else None
    collisions = (tuple(sparse(work, f"collision-{index}.raw", target_size) for index in range(2))
                  if args.target_bus == "ahci" else ())
    blank = sha(target)
    assert sha(rejected) == blank

    def launch(port, serial_log):
        return ubuntu_command(instance, target, rejected, port, serial_log, args.target_bus, collisions, replacement)

    with boot_ubuntu(instance, key, 1, launch=launch, readiness_timeout=600) as run:
        print("Ubuntu installer host provisioned from the verified Core package", flush=True)
        ubuntu = identity(run)
        run("test -d /sys/firmware/efi")
        package = remote(run, "dpkg-query", "-W", "-f=${Version}", "psios-core").stdout.strip()
        assert package == manifest["package_version"], package
        control = instance / "control.sock" if replacement is not None else None
        evidence, installed_digest = install_in_guest(run, args.tool_deb, work, image, receipt, source_digest,
                                                      blank, args.target_bus, target_size, control)
        assert evidence["boot_id"] == ubuntu[2], evidence
    if replacement is not None:
        assert sha(target) == blank, "the removed NVMe changed"
        target = replacement
    assert sha(image) == source_digest and sha(target) == installed_digest, "installed raw bytes differ after shutdown"
    assert sha(rejected) == blank, "refused block device changed"
    assert all(sha(disk) == blank for disk in collisions), "duplicate-identity disk changed"
    if args.larger_target:
        check_larger_disk(image, target)
    authority, task = boot_installed(args, work, target, receipt, operator, admin)
    assert authority != ubuntu[0], "the installed raw disk reused the Ubuntu authority"
    print(f"PASS {evidence['transport']['kind']} {evidence['transport']['serial']}, full raw image {installed_digest}, "
          f"Ubuntu kernel disk sequence {evidence['disk_sequence']}, native Task {task} survived reboot")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True, help="pinned Ubuntu minimal QCOW2")
    parser.add_argument("--package", type=Path, required=True, help="tested PsiOS Core amd64 DEB")
    parser.add_argument("--archive", type=Path, required=True, help="authenticated portable Core archive")
    parser.add_argument("--archive-sha256", required=True, help="expected digest of the portable Core archive")
    parser.add_argument("--kernel-iso", type=Path, required=True, help="pinned kernel ISO")
    parser.add_argument("--target-bus", choices=("virtio", "nvme", "ahci"), default="virtio")
    parser.add_argument("--larger-target", action="store_true", help="install to a larger, sector-aligned blank test disk")
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
