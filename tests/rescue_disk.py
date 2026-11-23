"""Boot the external rescue kernel against real, disposable raw ext4 images."""
import argparse
import hashlib
import json
import os
from pathlib import Path
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


def exercise(iso, binaries, inspector, root):
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

    def inspect(image, extra=(), rejection=None, journal=None):
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
            assert report == expected
        else:
            errors = (rejection,) if isinstance(rejection, str) else rejection
            assert result.returncode != 0 and not result.stdout and any(e in result.stderr for e in errors), result

    inspect(volume)
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
    inspect(disk, ["--partition", "1"])
    inspect(disk, ["--partition", "1", "--journal-overlay-mib", "16"], journal="Clean")
    inspect(disk, ["--partition", "2"], "block-device")

    # Set EXT4_FEATURE_INCOMPAT_RECOVER on a copy and update its superblock checksum.
    dirty = root / "dirty.raw"
    shutil.copyfile(volume, dirty)
    subprocess.run(["debugfs", "-w", "-R", "feature needs_recovery", dirty], check=True, capture_output=True)
    inspect(dirty, rejection="filesystem-recovery-required")
    inspect(dirty, ["--journal-overlay-mib", "16"], journal="ReplayedOnTemporaryOverlay")
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
          "active writer rejection, scratch cleanup and byte-for-byte source preservation.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--inspector", type=Path, default=INSPECT, help="launcher from the checkout or extracted archive")
    args = parser.parse_args()
    if not os.getuid():
        parser.error("run this acceptance test as a non-root service user")
    with tempfile.TemporaryDirectory(prefix="psios-disk-test-") as tmp:
        exercise(args.iso.resolve(), args.bin_dir.resolve(), args.inspector.resolve(), Path(tmp))
