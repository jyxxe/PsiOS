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
        report = json.loads(subprocess.check_output([*node.daemon, "verify", "--scratch-dir", node.base]))
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


def boot_cd(work, firmware, cd, drives, *, source_digest=None):
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
    output = subprocess.check_output(command, stdin=subprocess.DEVNULL, timeout=180).replace(b"\r\n", b"\n")
    assert output.count(MARKER) == 1, output
    boot_log, report = output.split(MARKER, 1)
    if source_digest is not None:
        assert f"\nPSIOS-SOURCE/1\n{source_digest}\n".encode() in boot_log, output
    return report.strip()


def exercise(args, work):
    identity = uuid.uuid4()
    source, expected = authority(work, args.bin_dir, "intended", identity)
    decoy, unrelated = authority(work, args.bin_dir, "different", uuid.uuid4())
    assert unrelated["node_id"] != expected["node_id"]
    duplicate = work / "duplicate.raw"
    shutil.copyfile(source, duplicate)
    inputs = {disk: digest(disk) for disk in (source, decoy, duplicate)}
    core = args.bin_dir / "psiosd"
    scratch = work / "scratch"
    scratch.mkdir()
    call = [sys.executable, args.inspector, source, "--iso", args.iso, "--core", core, "--core-sha256", digest(core),
            "--uid", str(os.getuid()), "--gid", str(os.getgid()), "--state", "/state", "--witness", "/witness",
            "--partition", "1", "--ovmf-dir", args.ovmf_dir]

    def inspect(*options, rejection=None, source_digest=None):
        if source_digest is not None:
            options += ("--source-sha256", source_digest)
        result = subprocess.run([*call, *options], text=True, capture_output=True, timeout=210,
                                env={**os.environ, "TMPDIR": str(scratch)})
        assert not list(scratch.iterdir()), "rescue staging leaked temporary files"
        assert digest(source) == inputs[source], "rescue changed the source image"
        if rejection is None:
            assert result.returncode == 0, result.stderr
            report = json.loads(result.stdout)
            if source_digest is not None:
                assert report.pop("source_sha256") == source_digest
            assert report == expected
        else:
            assert result.returncode != 0 and not result.stdout and rejection in result.stderr, result

    inspect("--disk-transport", "nvme", rejection="requires direct read-only inspection")
    inspect("--filesystem-uuid", "not-a-uuid", rejection="expected a nonzero ext4 UUID")
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(identity), "--firmware", "uefi",
            rejection="requires direct read-only inspection")
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(identity), source_digest=inputs[source])
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(identity), source_digest=inputs[decoy],
            rejection="source-digest-mismatch")
    inspect("--disk-transport", "nvme", "--filesystem-uuid", str(uuid.uuid4()), rejection="filesystem-identity")
    inspect("--filesystem-uuid", str(identity), "--partition", "2", rejection="filesystem-identity")

    cd = work / "pinned.iso"
    inspect("--filesystem-uuid", str(identity), "--firmware", "uefi", "--save-rescue-iso", cd,
            source_digest=inputs[source])
    before = digest(cd)
    report = boot_cd(work, args.ovmf_dir, cd, [(decoy, "on"), (source, "on")], source_digest=inputs[source])
    status, _, payload = report.partition(b"\n")
    assert status == b"OK" and json.loads(payload) == expected, report
    assert boot_cd(work, args.ovmf_dir, cd, [(decoy, "on")]) == b"ERROR filesystem-identity"
    assert boot_cd(work, args.ovmf_dir, cd, [(source, "on"), (duplicate, "off")]) == b"ERROR ambiguous-filesystem-identity"
    assert boot_cd(work, args.ovmf_dir, cd, [(decoy, "on"), (source, "off")]) == b"ERROR block-device"
    assert {disk: digest(disk) for disk in inputs} == inputs
    assert digest(cd) == before
    print("Verified exact authority on QEMU NVMe and reordered Virtio, GPT partition identity, selected whole-disk "
          "digest, missing and duplicate filesystem refusal, writable-device refusal and byte-for-byte preservation.")


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
