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


def exercise(iso, binaries, root):
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

    def inspect(image, extra=(), rejection=None):
        before = digest(image)
        result = subprocess.run([sys.executable, INSPECT, image, *options, *extra],
                                capture_output=True, text=True, timeout=210)
        assert digest(image) == before, "inspection changed the source image"
        if rejection is None:
            assert result.returncode == 0, result.stderr
            assert json.loads(result.stdout) == expected
        else:
            assert result.returncode != 0 and not result.stdout and rejection in result.stderr, result

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
    inspect(disk, ["--partition", "2"], "block-device")

    # Set EXT4_FEATURE_INCOMPAT_RECOVER on a copy and update its superblock checksum.
    dirty = root / "dirty.raw"
    shutil.copyfile(volume, dirty)
    subprocess.run(["debugfs", "-w", "-R", "feature needs_recovery", dirty], check=True, capture_output=True)
    inspect(dirty, rejection="filesystem-recovery-required")
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
        finally:
            owner.terminate()
    print("Verified standalone and GPT images, crash WAL, service identity, trust pins, copy limit, dirty filesystem, "
          "damaged witness, active writer rejection and byte-for-byte source preservation.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    args = parser.parse_args()
    if not os.getuid():
        parser.error("run this acceptance test as a non-root service user")
    with tempfile.TemporaryDirectory(prefix="psios-disk-test-") as tmp:
        exercise(args.iso.resolve(), args.bin_dir.resolve(), Path(tmp))
