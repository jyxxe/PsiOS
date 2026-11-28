"""Select the intended ext4 authority under a real rescue kernel."""
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

from rescue_disk import INSPECT, MIB, digest, image_file
from support import Node


MARKER = b"\nPSIOS-RESCUE/1\n"


def authority(work, binaries, name, identity):
    node = Node(binaries)
    try:
        node.call("Agent.Register", {"name": name})
        node.stop(signal.SIGKILL)
        assert (node.state / "state.db-wal").stat().st_size > 32
        report = json.loads(subprocess.check_output([*node.daemon, "verify"]))
        tree = work / name
        tree.mkdir(mode=0o755)
        shutil.copytree(node.state, tree / "state")
        shutil.copytree(node.witness, tree / "witness")
    finally:
        node.close()
    volume = work / f"{name}.ext4"
    image_file(volume, 32 * MIB)
    subprocess.run(["mkfs.ext4", "-q", "-U", str(identity), "-d", tree, volume], check=True)
    disk = work / f"{name}.raw"
    image_file(disk, 40 * MIB)
    subprocess.run(["sfdisk", "--quiet", disk], input="label: gpt\nstart=2048,size=65536,type=linux\n",
                   text=True, check=True)
    with volume.open("rb") as source, disk.open("r+b") as target:
        target.seek(MIB)
        shutil.copyfileobj(source, target)
    return disk, report


def boot_cd(work, firmware, cd, drives, *, sata=None):
    code = firmware / "OVMF_CODE_4M.fd"
    variables = work / "vars.fd"
    shutil.copyfile(firmware / "OVMF_VARS_4M.fd", variables)
    command = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-machine", "q35",
               "-m", "768", "-smp", "2", "-nic", "none", "-display", "none", "-monitor", "none",
               "-serial", "null", "-serial", "stdio", "-boot", "order=d,strict=on", "-cdrom", cd,
               "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
               "-drive", f"if=pflash,format=raw,unit=1,file={variables}"]
    for index, (disk, readonly) in enumerate(drives):
        command.extend(["-drive", f"if=none,format=raw,id=disk{index},file={disk},readonly={readonly}",
                        "-device", f"virtio-blk-pci,drive=disk{index},addr={index+4}"])
    if sata is not None:
        command.extend(["-drive", f"if=none,format=raw,id=sata,file={sata}",
                        "-device", "ich9-ahci,id=controller,addr=8",
                        "-device", "ide-hd,drive=sata,bus=controller.0"])
    output = subprocess.check_output(command, stdin=subprocess.DEVNULL, timeout=180).replace(b"\r\n", b"\n")
    assert output.count(MARKER) == 1, output
    return output.split(MARKER, 1)[1].strip()


def exercise(args, work):
    identity = uuid.uuid4()
    source, expected = authority(work, args.bin_dir, "intended", identity)
    decoy, unrelated = authority(work, args.bin_dir, "different", uuid.uuid4())
    assert unrelated["node_id"] != expected["node_id"]
    duplicate = work / "duplicate.raw"
    shutil.copyfile(source, duplicate)
    dirty = work / "dirty.raw"
    shutil.copyfile(source, dirty)
    volume = work / "intended.ext4"
    subprocess.run(["debugfs", "-w", "-R", "feature needs_recovery", volume], check=True, capture_output=True)
    with volume.open("rb") as source_fs, dirty.open("r+b") as dirty_disk:
        dirty_disk.seek(MIB)
        shutil.copyfileobj(source_fs, dirty_disk)
    inputs = {disk: digest(disk) for disk in (source, decoy, duplicate, dirty)}
    core = args.bin_dir / "psiosd"
    scratch = work / "scratch"
    scratch.mkdir()
    settings = ["--iso", args.iso, "--core", core, "--core-sha256", digest(core), "--uid", str(os.getuid()),
                "--gid", str(os.getgid()), "--state", "/state", "--witness", "/witness", "--partition", "1",
                "--ovmf-dir", args.ovmf_dir]

    def inspect(*options, rejection=None, image=source):
        result = subprocess.run([sys.executable, args.inspector, image, *settings, *options],
                                text=True, capture_output=True, timeout=210,
                                env={**os.environ, "TMPDIR": str(scratch)})
        assert not list(scratch.iterdir()), "rescue staging leaked temporary files"
        assert digest(image) == inputs[image], "rescue changed the source image"
        if rejection is None:
            assert result.returncode == 0, result.stderr
            assert json.loads(result.stdout) == expected
        else:
            assert result.returncode != 0 and not result.stdout and rejection in result.stderr, result

    inspect("--disk-transport", "nvme", rejection="direct read-only inspection")
    inspect("--filesystem-uuid", "not-a-uuid", rejection="expected a nonzero ext4 UUID")
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(identity), "--firmware", "uefi",
            rejection="direct read-only inspection")
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(identity))
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(uuid.uuid4()), rejection="filesystem-identity")
    inspect("--disk-transport", "ahci", "--filesystem-uuid", str(identity))
    inspect("--disk-transport", "ahci", "--filesystem-uuid", str(identity), image=dirty,
            rejection="filesystem-recovery-required")
    inspect("--disk-transport", "ahci", "--filesystem-uuid", str(identity), "--journal-overlay-mib", "16",
            rejection="direct read-only inspection")
    inspect("--filesystem-uuid", str(identity), "--partition", "2", rejection="filesystem-identity")

    cd = work / "pinned.iso"
    inspect("--filesystem-uuid", str(identity), "--firmware", "uefi", "--save-rescue-iso", cd)
    before = digest(cd)
    report = boot_cd(work, args.ovmf_dir, cd, [(decoy, "on"), (source, "on")])
    status, _, payload = report.partition(b"\n")
    assert status == b"OK" and json.loads(payload) == expected, report
    assert boot_cd(work, args.ovmf_dir, cd, [(decoy, "on")]) == b"ERROR filesystem-identity"
    assert boot_cd(work, args.ovmf_dir, cd, [(source, "on"), (duplicate, "off")]) == b"ERROR ambiguous-filesystem-identity"
    assert boot_cd(work, args.ovmf_dir, cd, [(source, "on")], sata=duplicate) == b"ERROR ambiguous-filesystem-identity"
    assert boot_cd(work, args.ovmf_dir, cd, [(decoy, "on")], sata=source) == b"ERROR block-device"
    assert boot_cd(work, args.ovmf_dir, cd, [(decoy, "on"), (source, "off")]) == b"ERROR block-device"
    assert {disk: digest(disk) for disk in inputs} == inputs
    assert digest(cd) == before
    print("Verified exact authority on QEMU NVMe, AHCI and reordered Virtio, GPT partition identity, missing and "
          "duplicate filesystem refusal across controllers, writable and dirty-source refusal, and byte preservation.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--inspector", type=Path, default=INSPECT)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    if not os.getuid():
        parser.error("run this acceptance test as a non-root service user")
    with tempfile.TemporaryDirectory(prefix="psios-rescue-storage-") as directory:
        exercise(args, Path(directory))
