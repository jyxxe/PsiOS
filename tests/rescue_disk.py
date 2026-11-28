"""Boot the external rescue kernel against real, disposable raw ext4 images."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

from support import Node


INSPECT = Path(__file__).resolve().parents[1] / "images/rescue/inspect.py"
MIB = 1024 * 1024


def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def image_file(path, size):
    with path.open("xb") as f:
        f.truncate(size)


def reject_source_changes(root, scratch, inspector, volume, options, firmware_media):
    host_qemu = shlex.quote(shutil.which("qemu-system-x86_64"))
    tools = root / "private rescue tools"
    tools.mkdir(mode=0o700)
    qemu = tools / "qemu-system-x86_64"
    env = {**os.environ, "TMPDIR": str(scratch), "PATH": str(tools) + os.pathsep + os.environ["PATH"]}
    expected = digest(volume)

    def refused(image, operation, extra=()):
        # Let the genuine rescue guest finish before changing its source.
        qemu.write_text(f'#!/bin/sh\nset -e\n{host_qemu} "$@"\n{operation}\n')
        qemu.chmod(0o700)
        result = subprocess.run([sys.executable, inspector, image, *options, "--source-sha256", expected, *extra],
                                capture_output=True, text=True, timeout=210, env=env)
        assert result.returncode != 0 and not result.stdout, result
        assert "rescue source image changed during inspection" in result.stderr, result.stderr
        assert not list(scratch.iterdir()) and not list(root.glob(".psios-media-*"))

    edited = root / "edited-after-guest.raw"
    shutil.copyfile(volume, edited)
    output = root / "changed-source.iso"
    media = ("--firmware", "bios", "--save-rescue-iso", output) if firmware_media else ()
    refused(edited, f"printf 'changed' >> {shlex.quote(str(edited))}", extra=media)
    assert digest(edited) != expected and not output.exists()

    renamed = root / "replaced-after-guest.raw"
    spare = root / "byte-identical-replacement.raw"
    shutil.copyfile(volume, renamed)
    shutil.copyfile(volume, spare)
    original_inode = renamed.stat().st_ino
    refused(renamed, shlex.join(("mv", "-f", "--", str(spare), str(renamed))))
    assert renamed.stat().st_ino != original_inode and digest(renamed) == expected
    assert digest(volume) == expected
    print("PASS genuine rescue guest results withheld after a source write or byte-identical pathname replacement", flush=True)


def exercise(iso, binaries, inspector, root, firmware_media):
    node = Node(binaries)
    try:
        identity = node.call("Node.Get", {})["reference"]["resource_id"]
        node.call("Agent.Register", {"name": "persist-in-wal"})
        node.stop(signal.SIGKILL)
        assert (node.state / "state.db-wal").stat().st_size > 32
        expected = json.loads(subprocess.check_output(node.daemon + ["verify"]))
        assert expected["node_id"] == identity
        volume = root / "volume"
        image_file(volume, 32 * MIB)
        tree = root / "tree"
        tree.mkdir()
        tree.chmod(0o755)
        shutil.copytree(node.state, tree / "state")
        shutil.copytree(node.witness, tree / "witness")
        # Leave a source-side executable where an unsafe inspector could find it.
        # The rescue boot must use the externally supplied, digest-checked binary.
        decoy = tree / "psiosd"
        decoy.write_text("#!/bin/sh\nexit 111\n")
        decoy.chmod(0o755)
        subprocess.run(["mkfs.ext4", "-q", "-d", tree, volume], check=True)
    finally:
        node.close()

    core = binaries / "psiosd"
    options = ["--iso", iso, "--core", core, "--core-sha256", digest(core),
               "--uid", str(os.getuid()), "--gid", str(os.getgid()), "--state", "/state", "--witness", "/witness"]
    scratch = root / "private rescue, scratch"
    scratch.mkdir(mode=0o700)

    def inspect(image, extra=(), rejection=None, journal=None, source_digest=None):
        before = digest(image)
        result = subprocess.run([sys.executable, inspector, image, *options, *extra],
                                capture_output=True, text=True, timeout=210, env={**os.environ, "TMPDIR": str(scratch)})
        assert digest(image) == before, "inspection changed the source image"
        assert not list(scratch.iterdir()), "temporary boot media or journal overlay left behind"
        if rejection is None:
            assert result.returncode == 0, result.stderr
            report = json.loads(result.stdout)
            if journal is not None:
                assert report.pop("ext4_journal") == journal
            if source_digest is not None:
                assert report.pop("source_sha256") == source_digest
            assert report == expected
        else:
            errors = (rejection,) if isinstance(rejection, str) else rejection
            assert result.returncode != 0 and not result.stdout and any(e in result.stderr for e in errors), result

    inspect(volume)
    source_digest = digest(volume)
    inspect(volume, ["--source-sha256", source_digest], source_digest=source_digest)
    inspect(volume, ["--source-sha256", "0" * 64], "source-digest-mismatch")
    inspect(volume, ["--source-sha256", "invalid"], "invalid expected source image SHA-256")
    reject_source_changes(root, scratch, inspector, volume, options, firmware_media)
    if firmware_media:
        bios = root / "BIOS rescue.iso"
        uefi = root / "UEFI rescue.iso"
        inspect(volume, ["--firmware", "bios", "--save-rescue-iso", bios])
        inspect(volume, ["--firmware", "uefi", "--save-rescue-iso", uefi])
        original = digest(bios)
        assert original == digest(uefi)
        inspect(volume, ["--firmware", "bios", "--save-rescue-iso", bios], "output already exists")
        assert digest(bios) == original
        inspect(volume, ["--firmware", "bios", "--journal-overlay-mib", "16"], "read-only block device")
        rejected = root / "unverified.iso"
        wrong_uid = 65532 if os.getuid() != 65532 else 65531
        inspect(volume, ["--firmware", "bios", "--uid", str(wrong_uid), "--save-rescue-iso", rejected],
                "authority-PermissionDenied")
        assert not rejected.exists()
        def boot_cd(cd, image, readonly):
            command = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot",
                       "-machine", "pc", "-m", "768", "-display", "none", "-monitor", "none", "-nic", "none",
                       "-serial", "null", "-serial", "stdio", "-boot", "order=d,strict=on", "-cdrom", cd,
                       "-drive", f"file={image},format=raw,if=virtio,readonly={readonly}"]
            return subprocess.check_output(command, stdin=subprocess.DEVNULL, timeout=180).replace(b"\r\n", b"\n")

        # Reboot the exported CDs without the build tree or launcher.
        assert boot_cd(bios, volume, "off").endswith(b"PSIOS-RESCUE/1\nERROR block-device\n")
        pinned = root / "pinned-source.iso"
        inspect(volume, ["--source-sha256", source_digest, "--firmware", "bios", "--save-rescue-iso", pinned],
                source_digest=source_digest)
        accepted = boot_cd(pinned, volume, "on")
        assert f"\nPSIOS-SOURCE/1\n{source_digest}\n".encode() in accepted, accepted
        assert json.loads(accepted.split(b"\nPSIOS-RESCUE/1\nOK\n", 1)[1]) == expected
        changed = root / "changed.raw"
        shutil.copyfile(volume, changed)
        subprocess.run(["debugfs", "-w", "-R", "mkdir /unrecorded", changed], check=True, capture_output=True)
        inspect(changed)
        modified = digest(changed)
        assert modified != source_digest
        refused = boot_cd(pinned, changed, "on")
        assert b"\nPSIOS-SOURCE/1\n" not in refused, refused
        assert refused.endswith(b"PSIOS-RESCUE/1\nERROR source-digest-mismatch\n"), refused
        assert digest(volume) == source_digest and digest(changed) == modified
        print(f"Verified SeaBIOS, OVMF and standalone read-only enforcement; identical ISO SHA-256 {original}",
              flush=True)
    other_uid = 65532 if os.getuid() != 65532 else 65531
    inspect(volume, ["--uid", str(other_uid)], "authority-PermissionDenied")
    inspect(volume, ["--core-sha256", "0" * 64], "SHA-256 mismatch")
    inspect(volume, ["--iso", volume], "SHA-256 mismatch")
    inspect(volume, ["--state", "/state/../witness"], "normalized absolute path")

    # Make a second filesystem with an oversized source journal; the copy limit
    # must fire before SQLite is given the content.
    with (tree / "state/state.db-wal").open("ab") as journal:
        journal.truncate(2 * MIB)
    limited = root / "limited.raw"
    image_file(limited, 32 * MIB)
    subprocess.run(["mkfs.ext4", "-q", "-d", tree, limited], check=True)
    inspect(limited, ["--max-copy-mib", "1"], "authority-QuotaExceeded")

    disk = root / "gpt.raw"
    image_file(disk, 36 * MIB)
    subprocess.run(["sfdisk", "--quiet", disk], input="label: gpt\nstart=2048,size=65536,type=linux\n",
                   text=True, check=True)
    with disk.open("r+b") as dst, volume.open("rb") as src:
        dst.seek(MIB)
        shutil.copyfileobj(src, dst)
    full_disk_digest = digest(disk)
    inspect(disk, ["--partition", "1", "--source-sha256", full_disk_digest], source_digest=full_disk_digest)
    inspect(disk, ["--partition", "1", "--source-sha256", source_digest], "source-digest-mismatch")
    inspect(disk, ["--partition", "1", "--journal-overlay-mib", "16"], journal="Clean")
    inspect(disk, ["--partition", "2"], "block-device")

    # Set EXT4_FEATURE_INCOMPAT_RECOVER on a copy and update its superblock checksum.
    dirty = root / "dirty.raw"
    shutil.copyfile(volume, dirty)
    subprocess.run(["debugfs", "-w", "-R", "feature needs_recovery", dirty], check=True, capture_output=True)
    inspect(dirty, rejection="filesystem-recovery-required")
    dirty_digest = digest(dirty)
    inspect(dirty, ["--journal-overlay-mib", "16", "--source-sha256", dirty_digest],
            journal="ReplayedOnTemporaryOverlay", source_digest=dirty_digest)
    subprocess.run(["debugfs", "-w", "-R", "set_super_value state 3", dirty], check=True, capture_output=True)
    inspect(dirty, ["--journal-overlay-mib", "16"], "filesystem-recovery-required")

    # The home block contains invalid credential bytes. Put its authentic bytes
    # only in an ext4 journal transaction. A committed transaction must restore
    # the verifier's evidence; an uncommitted one must never manufacture success.
    home_block = int(subprocess.check_output(["debugfs", "-R", "bmap /witness/owner.token 0", volume],
                                             stderr=subprocess.DEVNULL))
    with volume.open("rb") as f:
        f.seek(1024 + 24)  # s_log_block_size
        block_size = 1024 << int.from_bytes(f.read(4), "little")
        f.seek(block_size * home_block)
        original = f.read(block_size)
    assert node.owner.encode() in original
    saved = root / "journal-block"
    saved.write_bytes(original)
    for name, flag in [("committed", ""), ("uncommitted", "-c ")]:
        recovery = root / (name + ".raw")
        shutil.copyfile(volume, recovery)
        with recovery.open("r+b") as f:
            f.seek(block_size * home_block)
            f.write(bytes(block_size))
        # debugfs sets needs_recovery automatically for a committed transaction.
        # Model a writer that was still mounted when an uncommitted record was lost.
        commands = f"journal_open\njournal_write {flag}-b {home_block} journal-block\njournal_close\nfeature needs_recovery\n"
        subprocess.run(["debugfs", "-w", recovery], cwd=root, input=commands, text=True,
                       capture_output=True, check=True)
        inspect(recovery, rejection="filesystem-recovery-required")
        if name == "committed":
            inspect(recovery, ["--journal-overlay-mib", "16"], journal="ReplayedOnTemporaryOverlay")
        else:
            inspect(recovery, ["--journal-overlay-mib", "16"], "authority-")

    # Scatter real journal writes across 300 different 64 KiB overlay clusters.
    # The same recoverable filesystem must fail at 16 MiB and succeed at 32 MiB.
    wide = root / "wide.raw"
    shutil.copyfile(volume, wide)
    padding = root / "padding"
    with padding.open("wb") as f:
        for _ in range(21):
            f.write(b"P" * MIB)
    subprocess.run(["debugfs", "-w", "-R", "write padding /padding", wide], cwd=root,
                   check=True, capture_output=True)
    blocks = subprocess.check_output(["debugfs", "-R", "blocks /padding", wide], stderr=subprocess.DEVNULL)
    selected = {}
    with wide.open("rb") as disk_file:
        for block in map(int, blocks.split()):
            cluster = block * block_size // 65536
            if cluster not in selected:
                disk_file.seek(block * block_size)
                if disk_file.read(block_size) == b"P" * block_size:
                    selected[cluster] = block
            if len(selected) == 300:
                break
    assert len(selected) == 300
    (root / "journal-wide").write_bytes(b"Q" * (len(selected) * block_size))
    block_list = ",".join(map(str, selected.values()))
    commands = f"journal_open\njournal_write -b {block_list} journal-wide\njournal_close\n"
    subprocess.run(["debugfs", "-w", wide], cwd=root, input=commands, text=True, check=True, capture_output=True)
    inspect(wide, ["--journal-overlay-mib", "16"], rejection=("journal overlay file size limit", "journal-replay"))
    inspect(wide, ["--journal-overlay-mib", "32"], journal="ReplayedOnTemporaryOverlay")
    subprocess.run(["debugfs", "-w", "-R", "unlink /witness/keys", volume], check=True, capture_output=True)
    inspect(volume, rejection="authority-")

    # QMP readiness proves another VMM actually has a writable attachment.
    qmp = root / "owner.qmp"
    with subprocess.Popen(["qemu-system-x86_64", "-nodefaults", "-display", "none", "-monitor", "none",
                           "-S", "-m", "64", "-qmp", f"unix:{qmp},server=on,wait=off",
                           "-drive", f"file={disk},format=raw,if=virtio"], stderr=subprocess.DEVNULL) as owner:
        try:
            for _ in range(100):
                if qmp.exists():
                    break
                time.sleep(0.05)
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(5)
                sock.connect(str(qmp))
                assert "QMP" in json.loads(sock.makefile("rb").readline())
            inspect(disk, ["--partition", "1"], "lock")
            inspect(disk, ["--partition", "1", "--journal-overlay-mib", "16"], "lock")
        finally:
            owner.terminate()
    print("Verified raw and GPT images, crash WAL, service identity, trust pins, corrupt witness, "
          "committed and uncommitted ext4 journal recovery, filesystem errors, copy and overlay limits, "
          "active writer rejection, external source digest enforcement, scratch cleanup and byte-for-byte preservation.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--inspector", type=Path, default=INSPECT, help="launcher from the checkout or extracted archive")
    parser.add_argument("--firmware-media", action="store_true", help="also build, export and boot a BIOS/UEFI CD")
    args = parser.parse_args()
    if not os.getuid():
        parser.error("run this acceptance test as a non-root service user")
    with tempfile.TemporaryDirectory(prefix="psios-disk-test-") as tmp:
        exercise(args.iso.resolve(), args.bin_dir.resolve(), args.inspector.resolve(), Path(tmp), args.firmware_media)
