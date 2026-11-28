#!/usr/bin/env python3
"""Inspect a stopped PsiOS authority or native boot failure without booting the target OS."""
import argparse
from functools import partial
import json
import os
from pathlib import Path, PurePosixPath
import re
import resource
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
import boot_media
from boot_media import pinned_copy, regular_file


HERE = Path(__file__).resolve().parent
REPORT_LIMIT = 2 * 1024 * 1024
BOOT_TIMEOUT = 180
REPORT_MARKER = b"\nPSIOS-RESCUE/1\n"
SOURCE_MARKER = b"\nPSIOS-SOURCE/1\n"
AHCI_VIEW_MIB = 16


def scratch_mib(args):
    if type(args.max_copy_mib) is not int or not 1 <= args.max_copy_mib <= 256:
        raise ValueError("authority copy limit must be 1–256 MiB")
    # The copied source and migration's temporary pages can coexist. Reserve
    # another copy plus 64 MiB for journal and SQLite temporary-file overhead.
    return max(320, 2 * args.max_copy_mib + 64) if args.migration_config is not None else 320


def guest_path(value):
    path = PurePosixPath(value)
    if (not value.startswith("/") or value.startswith("//") or value == "/"
            or value != str(path) or ".." in path.parts or "\0" in value):
        raise argparse.ArgumentTypeError("expected a normalized absolute path inside the mounted filesystem")
    return value


def filesystem_uuid(value):
    try:
        identity = uuid.UUID(value)
        if identity.int == 0 or str(identity) != value.lower():
            raise ValueError()
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a nonzero ext4 UUID in standard form") from error
    return identity


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


def prepare(work, args, require_secure_boot, *, filesystem_uuid=None, disk_transport="virtio",
            source_sha256=None, boot_failure_only=False):
    scratch = scratch_mib(args)
    boot_media.prepare_kernel(work, args.iso)
    shutil.copyfile(HERE / "init", work / "psios-rescue")
    (work / "psios-rescue").chmod(0o755)
    members = ["psios-rescue"]
    if not boot_failure_only:
        pinned_copy(args.core, work / "psiosd", args.core_sha256, 64 * 1024 * 1024)
        (work / "psiosd").chmod(0o755)
        members.append("psiosd")
    # Keep input paths in JSON. The shell settings contain only validated literals.
    settings = (f"PARTITION={args.partition}\nSERVICE_UID={args.uid}\n"
                f"SERVICE_GID={args.gid if args.gid is not None else args.uid}\nCOPY_MIB={args.max_copy_mib}\n"
                f"SCRATCH_MIB={scratch}\nOVERLAY={int(args.journal_overlay_mib is not None)}\n"
                f"SECURE_BOOT={int(require_secure_boot)}\n"
                f"REHEARSE={int(args.migration_config is not None)}\n"
                f"FILESYSTEM_UUID={filesystem_uuid.hex if filesystem_uuid else ''}\n"
                f"DISK_TRANSPORT={disk_transport}\n"
                f"SOURCE_SHA256={source_sha256 or ''}\n"
                f"BOOT_FAILURE_ONLY={int(boot_failure_only)}\n")
    (work / "rescue-settings").write_text(settings)
    (work / "rescue-settings").chmod(0o600)
    members.append("rescue-settings")
    if not boot_failure_only:
        (work / "rescue.json").write_text(json.dumps(configuration(args), allow_nan=False))
        (work / "rescue.json").chmod(0o644)
        members.append("rescue.json")
    for name in members:
        os.utime(work / name, (boot_media.EPOCH, boot_media.EPOCH))
    with (work / "initrd").open("ab") as out:
        out.write(b"\0" * (-out.tell() % 4))
        out.flush()
        subprocess.run(["cpio", "--quiet", "-o", "-H", "newc", "--reproducible", "--owner=0:0"], cwd=work,
                       input=("\n".join(members) + "\n").encode("ascii"), stdout=out, check=True)


def capture_report(command, *, cwd, pass_fds=(), preexec_fn=None, timeout=BOOT_TIMEOUT, limit=REPORT_LIMIT):
    """Capture the report UART with one byte budget and one deadline for the child."""
    deadline = time.monotonic() + timeout
    serial = bytearray()
    with subprocess.Popen(command, cwd=cwd, pass_fds=pass_fds, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, preexec_fn=preexec_fn) as process:
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(process.stdout, selectors.EVENT_READ)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not ready.select(remaining):
                        raise subprocess.TimeoutExpired(command, timeout)
                    chunk = os.read(process.stdout.fileno(), min(8192, limit + 1 - len(serial)))
                    if not chunk:
                        break
                    serial.extend(chunk)
                    if len(serial) > limit:
                        raise ValueError(f"rescue QEMU report exceeded {limit} bytes")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(command, timeout)
            status = process.wait(timeout=remaining)
            return subprocess.CompletedProcess(command, status, bytes(serial))
        except subprocess.TimeoutExpired:
            raise subprocess.TimeoutExpired(command, timeout) from None
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def unique_report(serial, firmware, accelerator):
    # OVMF also mirrors its own boot messages to the report UART.
    output = serial.replace(b"\r\n", b"\n")
    count = output.count(REPORT_MARKER)
    if count != 1:
        raise ValueError("rescue inspection failed: no unique guest report "
                         f"(firmware={firmware}, accelerator={accelerator}, "
                         f"markers={count}, stdout_bytes={len(serial)})")
    return output


def native_boot_failure(body, filesystem, present):
    report = {"filesystem_uuid": str(filesystem), "boot_failure": {"status": "Missing"}}
    if not present and body == b"{}":
        return report
    if not present or not re.fullmatch(
        rb'\{"format":1,"boot_id":"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}",'
        rb'"stage":"[a-z][a-z0-9-]{0,63}"\}', body,
    ):
        raise ValueError("invalid native boot failure record")
    report["boot_failure"] = {"status": "Found", **json.loads(body)}
    return report


def inspect(args):
    with regular_file(args.image) as disk, tempfile.TemporaryDirectory(prefix="psios-rescue-") as tmp:
        size = os.fstat(disk.fileno()).st_size
        if args.source_sha256 is not None and (size == 0 or size % 512 != 0):
            raise ValueError("a digest-pinned raw image must consist of complete 512-byte sectors")
        work = Path(tmp)
        signing = None
        if args.secure_boot_key:
            signing = boot_media.prepare_secure_boot(work, args.secure_boot_key, args.secure_boot_cert,
                                                    args.efi_stub, args.efi_stub_sha256)
        prepare(work, args, signing is not None, filesystem_uuid=args.filesystem_uuid,
                disk_transport=args.disk_transport, source_sha256=args.source_sha256,
                boot_failure_only=args.boot_failure_only)
        # Hand QEMU the exact already-open read-only file. Raw format disables
        # backing-file discovery and the guest gets no interface to the network.
        fd = disk.fileno()
        accelerator = "kvm" if signing is None and os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
        if args.firmware == "direct":
            boot = ["-machine", "pc", "-kernel", "kernel", "-initrd", "initrd", "-append", boot_media.CMDLINE]
        else:
            boot_media.build(work, signing)
            boot = boot_media.firmware_args(args.firmware, work, args.ovmf_dir, args.ovmf_vars)
            boot += ["-cdrom", "rescue.iso", "-boot", "order=d,strict=on"]
        guest_mib = max(768, scratch_mib(args) + 384)
        command = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot",
                   *boot, "-accel", accelerator, "-m", str(guest_mib), "-smp", "2",
                   "-display", "none", "-monitor", "none", "-nic", "none", "-serial", "null", "-serial", "stdio",
                   "-add-fd", f"fd={fd},set=1", "-blockdev", "driver=file,node-name=disk,filename=/dev/fdset/1,read-only=on,locking=on",
                   "-blockdev", "driver=raw,node-name=source,file=disk,read-only=on"]
        drive = "source"
        limit_child = None
        overlay_mib = args.journal_overlay_mib
        if args.disk_transport == "ahci":
            # QEMU's ATA disk requires a writable top-level node. Its source
            # remains behind the same read-only raw and file nodes.
            overlay_mib = AHCI_VIEW_MIB
        if overlay_mib is not None:
            # Limit the length of the host's QCOW2 file, including its metadata.
            # A guest virtual disk can be much larger. Inherit stricter host limits.
            limit = overlay_mib * 1024 * 1024
            soft, _ = resource.getrlimit(resource.RLIMIT_FSIZE)
            if soft != resource.RLIM_INFINITY:
                limit = min(soft, limit)

            limit_child = partial(resource.setrlimit, resource.RLIMIT_FSIZE, (limit, limit))
            scratch = str(work / "source-view.qcow2")
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", scratch, str(os.fstat(fd).st_size)],
                           preexec_fn=limit_child, timeout=20, check=True)
            command += ["-blockdev", json.dumps({"driver": "file", "node-name": "changes", "filename": scratch}),
                        "-blockdev", "driver=qcow2,node-name=view,file=changes,backing=source"]
            drive = "view"
        if args.disk_transport == "nvme":
            command += ["-device", "nvme,id=controller,serial=PSIOS-RESCUE",
                        "-device", f"nvme-ns,drive={drive},bus=controller,nsid=1"]
        elif args.disk_transport == "ahci":
            command += ["-device", "ich9-ahci,id=controller",
                        "-device", f"ide-hd,drive={drive},bus=controller.0,werror=report,rerror=report"]
        else:
            command += ["-device", f"virtio-blk-pci,drive={drive},werror=report,rerror=report"]
        result = capture_report(command, cwd=work, pass_fds=(fd,), preexec_fn=limit_child)
        if limit_child is not None and result.returncode == -signal.SIGXFSZ:
            purpose = "AHCI inspection" if args.disk_transport == "ahci" else "journal"
            raise ValueError(f"{purpose} overlay file size limit reached")
        result.check_returncode()
        output = unique_report(result.stdout, args.firmware, accelerator)
        status, _, body = output.partition(REPORT_MARKER)[2].strip().partition(b"\n")
        success = {b"OK": None} if args.journal_overlay_mib is None else {
            b"OK clean": "Clean", b"OK replayed": "ReplayedOnTemporaryOverlay"}
        if args.boot_failure_only:
            success = {b"OK found": True, b"OK missing": False}
        if status.strip() not in success:
            failure = re.fullmatch(rb"ERROR ([A-Za-z0-9-]+)", status.strip())
            raise ValueError("rescue inspection failed: " + (failure[1].decode() if failure else "no valid report"))
        if args.boot_failure_only:
            report = native_boot_failure(body, args.filesystem_uuid, success[status.strip()])
        else:
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
        if args.source_sha256 is not None:
            evidence = SOURCE_MARKER + args.source_sha256.encode("ascii") + b"\n"
            if output.count(SOURCE_MARKER) != 1 or evidence not in output:
                raise ValueError("rescue guest did not verify the pinned source image digest")
            report["source_sha256"] = args.source_sha256
        if args.journal_overlay_mib is not None:
            report["ext4_journal"] = success[status.strip()]
        if args.save_rescue_iso is not None:
            boot_media.publish(work / "rescue.iso", args.save_rescue_iso)
        return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path)
    parser.add_argument("--iso", type=Path, required=True, help="pinned Alpine virt rescue ISO")
    parser.add_argument("--core", type=Path, help="standalone static psiosd for authority verification")
    parser.add_argument("--core-sha256", help="expected psiosd digest from a trusted build manifest")
    parser.add_argument("--source-sha256", help="expected SHA-256 of the complete raw source from an independent record")
    parser.add_argument("--uid", type=int, help="numeric UID of the original service account")
    parser.add_argument("--gid", type=int, help="original service GID, defaults to UID")
    parser.add_argument("--partition", type=int, default=0, help="partition number, or 0 for a raw filesystem")
    parser.add_argument("--filesystem-uuid", type=filesystem_uuid, metavar="UUID",
                        help="require a unique ext4 UUID at the given partition number")
    parser.add_argument("--disk-transport", choices=("virtio", "nvme", "ahci"), default="virtio",
                        help="QEMU attachment for the image (default virtio)")
    parser.add_argument("--state", type=guest_path, default="/var/lib/psios")
    parser.add_argument("--witness", type=guest_path, help="defaults to STATE.witness")
    parser.add_argument("--boot-failure-only", action="store_true",
                        help="read the native boot failure from a stopped image without starting Core")
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
    if args.boot_failure_only:
        if args.partition != 2 or args.filesystem_uuid is None:
            parser.error("native boot failure inspection requires --partition 2 and --filesystem-uuid")
        if (any(option is not None for option in (args.core, args.core_sha256, args.uid, args.gid, args.witness,
                                                   args.migration_config, args.journal_overlay_mib))
                or args.state != "/var/lib/psios" or args.max_copy_mib != 64):
            parser.error("native boot failure inspection reads a clean source using its fixed state layout")
    else:
        if args.core is None or args.core_sha256 is None or args.uid is None:
            parser.error("authority verification requires --core, --core-sha256 and --uid")
        if not re.fullmatch("[0-9a-f]{64}", args.core_sha256):
            parser.error("invalid expected SHA-256")
        if not 1 <= args.uid < 2**32 - 1 or args.gid is not None and not 0 <= args.gid < 2**32 - 1:
            parser.error("expected a non-root service UID and a valid GID")
    if args.source_sha256 is not None and not re.fullmatch("[0-9a-f]{64}", args.source_sha256):
        parser.error("invalid expected source image SHA-256")
    if not 0 <= args.partition <= 128 or not 1 <= args.max_copy_mib <= 256:
        parser.error("invalid partition or copy limit")
    if args.journal_overlay_mib is not None and not 16 <= args.journal_overlay_mib <= 512:
        parser.error("journal overlay limit must be 16–512 MiB")
    if args.firmware != "direct" and args.journal_overlay_mib is not None:
        parser.error("firmware media require a read-only block device")
    if args.disk_transport != "virtio" and (args.firmware != "direct" or args.journal_overlay_mib is not None
                                           or args.filesystem_uuid is None):
        parser.error("NVMe and AHCI require direct read-only inspection and --filesystem-uuid")
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
