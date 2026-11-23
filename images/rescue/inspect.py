#!/usr/bin/env python3
"""Inspect a stopped PsiOS authority on a raw disk image without booting its OS."""
import argparse
from functools import partial
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import resource
import shutil
import signal
import stat
import subprocess
import sys
import tempfile


HERE = Path(__file__).resolve().parent


def regular_file(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError(f"expected a regular file: {path}")
    return os.fdopen(fd, "rb")


def pinned_copy(source, target, expected, limit):
    digest = hashlib.sha256()
    with regular_file(source) as src, target.open("xb") as dst:
        while chunk := src.read(1024 * 1024):
            limit -= len(chunk)
            if limit < 0:
                raise ValueError(f"input too large: {source}")
            digest.update(chunk)
            dst.write(chunk)
    if digest.hexdigest() != expected:
        raise ValueError(f"SHA-256 mismatch: {source}")


def guest_path(value):
    path = PurePosixPath(value)
    if (not value.startswith("/") or value.startswith("//") or value == "/"
            or value != str(path) or ".." in path.parts or "\0" in value):
        raise argparse.ArgumentTypeError("expected a normalized absolute path inside the mounted filesystem")
    return value


def prepare(work, args):
    base = json.loads((HERE / "base.json").read_text())
    iso = work / "base.iso"
    pinned_copy(args.iso, iso, base["sha256"], base["size_bytes"])
    for name, member, digest in [("kernel", "vmlinuz-virt", "kernel_sha256"),
                                 ("initrd", "initramfs-virt", "initramfs_sha256")]:
        path = work / name
        with path.open("wb") as out:
            subprocess.run(["isoinfo", "-R", "-i", iso, "-x", "/boot/" + member], stdout=out, check=True)
        with path.open("rb") as f:
            if hashlib.file_digest(f, "sha256").hexdigest() != base[digest]:
                raise ValueError(f"invalid {member} in boot media")
    pinned_copy(args.core, work / "psiosd", args.core_sha256, 64 * 1024 * 1024)
    shutil.copyfile(HERE / "init", work / "psios-rescue")
    for name in ["psiosd", "psios-rescue"]:
        (work / name).chmod(0o755)
    # Keep input paths in JSON. The init script sources only these numeric settings.
    settings = (f"PARTITION={args.partition}\nSERVICE_UID={args.uid}\n"
                f"SERVICE_GID={args.gid if args.gid is not None else args.uid}\nCOPY_MIB={args.max_copy_mib}\n"
                f"OVERLAY={int(args.journal_overlay_mib is not None)}\n")
    (work / "rescue-settings").write_text(settings)
    config = {"state_dir": "/source" + args.state, "witness_dir": "/source" + (args.witness or args.state + ".witness"),
              "socket": "/run/rescue.sock"}
    (work / "rescue.json").write_text(json.dumps(config))
    (work / "rescue.json").chmod(0o644)
    with (work / "initrd").open("ab") as out:
        out.write(b"\0" * (-out.tell() % 4))
        out.flush()
        subprocess.run(["cpio", "--quiet", "-o", "-H", "newc", "--owner=0:0"], cwd=work,
                       input=b"psios-rescue\npsiosd\nrescue-settings\nrescue.json\n", stdout=out, check=True)


def inspect(args):
    with regular_file(args.image) as disk, tempfile.TemporaryDirectory(prefix="psios-rescue-") as tmp:
        work = Path(tmp)
        prepare(work, args)
        # Hand QEMU the exact already-open read-only file. Raw format disables
        # backing-file discovery and the guest gets no interface to the network.
        fd = disk.fileno()
        kvm = os.access("/dev/kvm", os.R_OK | os.W_OK)
        command = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot",
                   "-machine", "pc", "-accel", "kvm" if kvm else "tcg", "-m", "768", "-smp", "2",
                   "-display", "none", "-monitor", "none", "-nic", "none", "-serial", "null", "-serial", "stdio",
                   "-kernel", str(work / "kernel"), "-initrd", str(work / "initrd"),
                   "-append", "console=ttyS0 quiet panic=1 rdinit=/psios-rescue",
                   "-add-fd", f"fd={fd},set=1", "-blockdev", "driver=file,node-name=disk,filename=/dev/fdset/1,read-only=on,locking=on",
                   "-blockdev", "driver=raw,node-name=source,file=disk,read-only=on"]
        drive = "source"
        limit_child = None
        if args.journal_overlay_mib is not None:
            # Limit the length of the host's QCOW2 file, including its metadata.
            # A guest virtual disk can be much larger. Inherit stricter host limits.
            limit = args.journal_overlay_mib * 1024 * 1024
            soft, _ = resource.getrlimit(resource.RLIMIT_FSIZE)
            if soft != resource.RLIM_INFINITY:
                limit = min(soft, limit)

            limit_child = partial(resource.setrlimit, resource.RLIMIT_FSIZE, (limit, limit))
            scratch = str(work / "journal.qcow2")
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", scratch, str(os.fstat(fd).st_size)],
                           preexec_fn=limit_child, timeout=20, check=True)
            command += ["-blockdev", json.dumps({"driver": "file", "node-name": "changes", "filename": scratch}),
                        "-blockdev", "driver=qcow2,node-name=view,file=changes,backing=source"]
            drive = "view"
        command += ["-device", f"virtio-blk-pci,drive={drive},werror=report,rerror=report"]
        result = subprocess.run(command, pass_fds=(fd,), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                preexec_fn=limit_child, timeout=180)
        if limit_child is not None and result.returncode == -signal.SIGXFSZ:
            raise ValueError("journal overlay file size limit reached")
        result.check_returncode()
        status, _, body = result.stdout.strip().partition(b"\n")
        success = {b"OK": None} if args.journal_overlay_mib is None else {
            b"OK clean": "Clean", b"OK replayed": "ReplayedOnTemporaryOverlay"}
        if status.strip() not in success:
            failure = re.fullmatch(rb"ERROR ([A-Za-z0-9-]+)", status.strip())
            raise ValueError("rescue inspection failed: " + (failure[1].decode() if failure else "no valid report"))
        report = json.loads(body)
        if report["sqlite_integrity"] != "Verified" or report["foreign_keys"] != "Verified":
            raise ValueError("incomplete authority verification")
        if args.journal_overlay_mib is not None:
            report["ext4_journal"] = success[status.strip()]
        return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path)
    parser.add_argument("--iso", type=Path, required=True, help="pinned Alpine virt rescue ISO")
    parser.add_argument("--core", type=Path, required=True, help="standalone static psiosd")
    parser.add_argument("--core-sha256", required=True, help="expected psiosd digest from a trusted build manifest")
    parser.add_argument("--uid", type=int, required=True, help="numeric UID of the original service account")
    parser.add_argument("--gid", type=int, help="original service GID, defaults to UID")
    parser.add_argument("--partition", type=int, default=0, help="partition number, or 0 for a raw filesystem")
    parser.add_argument("--state", type=guest_path, default="/var/lib/psios")
    parser.add_argument("--witness", type=guest_path, help="defaults to STATE.witness")
    parser.add_argument("--max-copy-mib", type=int, default=64, help="database and journal copy limit (1–256 MiB)")
    parser.add_argument("--journal-overlay-mib", type=int, metavar="MIB",
                        help="allow ext4 journal replay on a disposable bounded overlay (16–512 MiB)")
    args = parser.parse_args()
    if not re.fullmatch("[0-9a-f]{64}", args.core_sha256):
        parser.error("invalid expected SHA-256")
    if not 1 <= args.uid < 2**32 - 1 or args.gid is not None and not 0 <= args.gid < 2**32 - 1:
        parser.error("expected a non-root service UID and a valid GID")
    if not 0 <= args.partition <= 128 or not 1 <= args.max_copy_mib <= 256:
        parser.error("invalid partition or copy limit")
    if args.journal_overlay_mib is not None and not 16 <= args.journal_overlay_mib <= 512:
        parser.error("journal overlay limit must be 16–512 MiB")
    print(json.dumps(inspect(args), ensure_ascii=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
