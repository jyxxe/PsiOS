#!/usr/bin/env python3
"""Inspect a stopped PsiOS authority on a raw disk image without booting its OS."""
import argparse
from functools import partial
import json
import os
from pathlib import Path, PurePosixPath
import re
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import boot_media
from boot_media import pinned_copy, regular_file


HERE = Path(__file__).resolve().parent


def guest_path(value):
    path = PurePosixPath(value)
    if (not value.startswith("/") or value.startswith("//") or value == "/"
            or value != str(path) or ".." in path.parts or "\0" in value):
        raise argparse.ArgumentTypeError("expected a normalized absolute path inside the mounted filesystem")
    return value


def configuration(args):
    config = {"socket": "/run/rescue.sock"}
    if args.migration_config is not None:
        def unique_fields(items):
            result = dict(items)
            if len(result) != len(items):
                raise ValueError("duplicate field in migration configuration")
            return result

        with regular_file(args.migration_config) as source:
            data = source.read(64 * 1024 + 1)
        if len(data) > 64 * 1024:
            raise ValueError("migration configuration exceeds size limit")
        config = json.loads(data, object_pairs_hook=unique_fields)
        if not isinstance(config, dict):
            raise ValueError("migration configuration must be an object")
    return {**config, "state_dir": "/source" + args.state,
            "witness_dir": "/source" + (args.witness or args.state + ".witness")}


def prepare(work, args, require_secure_boot):
    boot_media.prepare_kernel(work, args.iso)
    pinned_copy(args.core, work / "psiosd", args.core_sha256, 64 * 1024 * 1024)
    shutil.copyfile(HERE / "init", work / "psios-rescue")
    for name in ["psiosd", "psios-rescue"]:
        (work / name).chmod(0o755)
    # Keep input paths in JSON. The init script sources only these numeric settings.
    settings = (f"PARTITION={args.partition}\nSERVICE_UID={args.uid}\n"
                f"SERVICE_GID={args.gid if args.gid is not None else args.uid}\nCOPY_MIB={args.max_copy_mib}\n"
                f"OVERLAY={int(args.journal_overlay_mib is not None)}\nSECURE_BOOT={int(require_secure_boot)}\n"
                f"REHEARSE={int(args.migration_config is not None)}\n")
    (work / "rescue-settings").write_text(settings)
    (work / "rescue-settings").chmod(0o600)
    (work / "rescue.json").write_text(json.dumps(configuration(args), allow_nan=False))
    (work / "rescue.json").chmod(0o644)
    for name in ("psios-rescue", "psiosd", "rescue-settings", "rescue.json"):
        os.utime(work / name, (boot_media.EPOCH, boot_media.EPOCH))
    with (work / "initrd").open("ab") as out:
        out.write(b"\0" * (-out.tell() % 4))
        out.flush()
        subprocess.run(["cpio", "--quiet", "-o", "-H", "newc", "--reproducible", "--owner=0:0"], cwd=work,
                       input=b"psios-rescue\npsiosd\nrescue-settings\nrescue.json\n", stdout=out, check=True)


def inspect(args):
    with regular_file(args.image) as disk, tempfile.TemporaryDirectory(prefix="psios-rescue-") as tmp:
        work = Path(tmp)
        signing = None
        if args.secure_boot_key:
            signing = boot_media.prepare_secure_boot(work, args.secure_boot_key, args.secure_boot_cert,
                                                    args.efi_stub, args.efi_stub_sha256)
        prepare(work, args, signing is not None)
        # Hand QEMU the exact already-open read-only file. Raw format disables
        # backing-file discovery and the guest gets no interface to the network.
        fd = disk.fileno()
        kvm = signing is None and os.access("/dev/kvm", os.R_OK | os.W_OK)
        if args.firmware == "direct":
            boot = ["-machine", "pc", "-kernel", "kernel", "-initrd", "initrd", "-append", boot_media.CMDLINE]
        else:
            boot_media.build(work, signing)
            boot = boot_media.firmware_args(args.firmware, work, args.ovmf_dir, args.ovmf_vars)
            boot += ["-cdrom", "rescue.iso", "-boot", "order=d,strict=on"]
        command = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot",
                   *boot, "-accel", "kvm" if kvm else "tcg", "-m", "768", "-smp", "2",
                   "-display", "none", "-monitor", "none", "-nic", "none", "-serial", "null", "-serial", "stdio",
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
        result = subprocess.run(command, cwd=work, pass_fds=(fd,), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                preexec_fn=limit_child, timeout=180)
        if limit_child is not None and result.returncode == -signal.SIGXFSZ:
            raise ValueError("journal overlay file size limit reached")
        result.check_returncode()
        # OVMF also mirrors its own boot messages to the report UART.
        output = result.stdout.replace(b"\r\n", b"\n")
        marker = b"\nPSIOS-RESCUE/1\n"
        if output.count(marker) != 1:
            raise ValueError("rescue inspection failed: no unique guest report")
        status, _, body = output.partition(marker)[2].strip().partition(b"\n")
        success = {b"OK": None} if args.journal_overlay_mib is None else {
            b"OK clean": "Clean", b"OK replayed": "ReplayedOnTemporaryOverlay"}
        if status.strip() not in success:
            failure = re.fullmatch(rb"ERROR ([A-Za-z0-9-]+)", status.strip())
            raise ValueError("rescue inspection failed: " + (failure[1].decode() if failure else "no valid report"))
        report = json.loads(body)
        if report["sqlite_integrity"] != "Verified" or report["foreign_keys"] != "Verified":
            raise ValueError("incomplete authority verification")
        if args.migration_config is not None:
            probe = report.get("migration_rehearsal", {})
            source, target = probe.get("source_format"), probe.get("target_format")
            if (probe.get("status") != "Verified" or type(source) is not int or type(target) is not int
                    or source != report["database_format"] or target < source
                    or report["migration_required"] is not (target > source)):
                raise ValueError("incomplete migration rehearsal")
        if signing:
            if output.count(b"\nPSIOS-SECURE-BOOT/1\nEnforced\n") != 1:
                raise ValueError("rescue guest did not confirm Secure Boot")
            report["secure_boot"] = {"status": "Enforced", **signing.receipt()}
        if args.journal_overlay_mib is not None:
            report["ext4_journal"] = success[status.strip()]
        if args.save_rescue_iso is not None:
            boot_media.publish(work / "rescue.iso", args.save_rescue_iso)
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
    parser.add_argument("--migration-config", type=Path, metavar="CORE_JSON",
                        help="rehearse opening a private authority copy with the staged Core and deployment configuration")
    parser.add_argument("--journal-overlay-mib", type=int, metavar="MIB",
                        help="allow ext4 journal replay on a disposable bounded overlay (16–512 MiB)")
    parser.add_argument("--firmware", choices=["direct", "bios", "uefi"], default="direct",
                        help="direct kernel boot or boot an independently built El Torito rescue CD")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"), help="trusted 4 MiB OVMF pair for UEFI")
    parser.add_argument("--ovmf-vars", type=Path, help="pre-enrolled 4 MiB OVMF variable store for signed UEFI rescue")
    boot_media.add_signing_arguments(parser)
    parser.add_argument("--save-rescue-iso", type=Path, metavar="NEW_FILE",
                        help="publish the tested optical medium after successful firmware inspection")
    args = parser.parse_args()
    signing = boot_media.signing_requested(args, parser)
    if signing != (args.ovmf_vars is not None) or (signing and args.firmware != "uefi"):
        parser.error("signed rescue requires --firmware uefi and a pre-enrolled --ovmf-vars store")
    if not re.fullmatch("[0-9a-f]{64}", args.core_sha256):
        parser.error("invalid expected SHA-256")
    if not 1 <= args.uid < 2**32 - 1 or args.gid is not None and not 0 <= args.gid < 2**32 - 1:
        parser.error("expected a non-root service UID and a valid GID")
    if not 0 <= args.partition <= 128 or not 1 <= args.max_copy_mib <= 256:
        parser.error("invalid partition or copy limit")
    if args.journal_overlay_mib is not None and not 16 <= args.journal_overlay_mib <= 512:
        parser.error("journal overlay limit must be 16–512 MiB")
    if args.firmware != "direct" and args.journal_overlay_mib is not None:
        parser.error("firmware media require a read-only block device")
    if args.migration_config is not None and (args.firmware != "direct" or args.journal_overlay_mib is not None):
        parser.error("migration rehearsal requires direct boot and a clean read-only disk")
    if args.save_rescue_iso is not None:
        if args.firmware == "direct":
            parser.error("saving an ISO requires bios or uefi firmware inspection")
        if os.path.lexists(args.save_rescue_iso):
            parser.error("output already exists")
    print(json.dumps(inspect(args), ensure_ascii=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
