#!/usr/bin/env python3
"""Install a verified PsiOS native disk onto an explicitly approved blank block device."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import sys
from tempfile import TemporaryDirectory
import uuid

from install_source import MAX_TARGET_BYTES, SECTOR_BYTES, fit_image, stage_image
from install_target import filesystem_type, open_target, read_target, require_reboot_target, require_unallocated


JOURNAL = "operation.json"
JOURNAL_LIMIT = 64 * 1024
BLOCK_SIZE = 1024 * 1024
ZERO = bytes(BLOCK_SIZE)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def timestamp():
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def digest(value):
    return hashlib.sha256(encode(value)).hexdigest()


def checked_digest(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value),
            "expected a lowercase SHA-256 digest")
    return value


@contextmanager
def lock_directory(path, *, owner=0):
    parts = path.absolute().parts[1:]
    require(parts and ".." not in parts, "use a dedicated absolute installation state directory")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open("/", flags)
    try:
        trusted_owners = (0, owner)
        for component in parts:
            parent = os.fstat(fd)
            require(parent.st_uid in trusted_owners and (not parent.st_mode & 0o022 or parent.st_mode & stat.S_ISVTX),
                    "state path must have protected root-owned parent directories" if owner == 0
                    else "state path must have protected parent directories")
            child = os.open(component, flags, dir_fd=fd)
            try:
                require(os.fstat(child).st_uid in trusted_owners, "state path has an untrusted directory owner")
                os.fsync(fd)
            except BaseException:
                os.close(child)
                raise
            os.close(fd)
            fd = child
        state = os.fstat(fd)
        require(state.st_uid == owner and stat.S_IMODE(state.st_mode) == 0o700,
                "state directory must have mode 0700 and belong to the operator")
        require(filesystem_type(fd) in (b"ext4", b"xfs", b"btrfs"),
                "installation state requires a supported persistent local filesystem (ext4, XFS or Btrfs)")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield fd
    finally:
        os.close(fd)


def valid_instant(value):
    return isinstance(value, str) and re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z", value)


def expected_image(plan):
    return plan.get("deployment_image", plan["source"])


def read_operation(directory):
    fd = os.open(JOURNAL, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                 dir_fd=directory)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_uid == 0
                and stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1,
                "untrusted installation journal")
        payload = stream.read(JOURNAL_LIMIT + 1)
    require(len(payload) <= JOURNAL_LIMIT, "installation journal exceeds its size limit")
    entry = json.loads(payload)
    require(isinstance(entry, dict) and type(entry.get("format")) is int and entry["format"] == 1
            and entry.get("status") in ("Planned", "WriteIssued", "Verified")
            and isinstance(entry.get("plan"), dict)
            and valid_instant(entry.get("planned_at"))
            and entry.get("plan_digest") == digest(entry["plan"]),
            "installation journal is invalid")
    plan = entry["plan"]
    source = plan.get("source")
    target = plan.get("target")
    require(plan.get("action") == "FreshNativeInstall" and type(plan.get("allow_loop")) is bool
            and isinstance(source, dict) and isinstance(target, dict)
            and all(isinstance(item, str) and Path(item).is_absolute()
                    for item in (plan.get("source_image"), plan.get("receipt"), target.get("path")))
            and type(source.get("size_bytes")) is int and 0 < source["size_bytes"] <= 8 * 1024 ** 3
            and type(target.get("size_bytes")) is int,
            "installation journal has an invalid plan")
    size = target["size_bytes"]
    if "deployment_image" in plan:
        image = plan["deployment_image"]
        require(isinstance(image, dict) and set(image) == {"size_bytes", "image_sha256", "backup_gpt_offset"}
                and type(image["size_bytes"]) is int and image["size_bytes"] == size
                and source["size_bytes"] < size <= MAX_TARGET_BYTES and size % SECTOR_BYTES == 0
                and type(image["backup_gpt_offset"]) is int and image["backup_gpt_offset"] % SECTOR_BYTES == 0
                and 2 * SECTOR_BYTES <= size - image["backup_gpt_offset"] <= BLOCK_SIZE,
                "installation journal has an invalid relocated image")
        checked_digest(image["image_sha256"])
    else:
        require(size == source["size_bytes"], "installation journal has an invalid target size")
    for value in (source.get("image_sha256"), source.get("receipt_sha256"), plan.get("blank_sha256")):
        checked_digest(value)
    if entry["status"] in ("WriteIssued", "Verified"):
        require(valid_instant(entry.get("write_issued_at")), "installation journal lacks a write witness")
    if entry["status"] == "Verified":
        require(valid_instant(entry.get("verified_at"))
                and entry.get("image_readback_sha256") == expected_image(plan)["image_sha256"]
                and type(entry.get("reconciled")) is bool,
                "installation journal lacks a verified receipt")
    if "reconciled_target" in entry:
        require(entry["status"] == "Verified" and entry["reconciled"]
                and isinstance(entry["reconciled_target"], dict),
                "installation journal has invalid cross-boot evidence")
        require_reboot_target(target, entry["reconciled_target"])
    return entry


def save_operation(directory, entry):
    payload = encode(entry) + b"\n"
    require(len(payload) <= JOURNAL_LIMIT, "installation journal exceeds its size limit")
    temporary = ".install-" + uuid.uuid4().hex
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                 0o600, dir_fd=directory)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, JOURNAL, src_dir_fd=directory, dst_dir_fd=directory)
        os.fsync(directory)
    finally:
        try:
            os.unlink(temporary, dir_fd=directory)
        except FileNotFoundError:
            pass


@contextmanager
def staged_source(directory, image, receipt, receipt_sha256, *, fit_size=None):
    # Keep the scratch path bound to the locked directory even if its name moves.
    anchor = Path(f"/proc/{os.getpid()}/fd/{directory}")
    fd = None
    try:
        with TemporaryDirectory(dir=anchor, prefix=".install-source-") as work:
            path, facts = stage_image(Path(work), Path(image), Path(receipt), receipt_sha256)
            if fit_size is not None:
                require(fit_size >= facts["size_bytes"], "target must fit all factory image partitions")
            fitted = None
            if fit_size is not None and fit_size != facts["size_bytes"]:
                fitted = fit_image(Path(work), path, facts, fit_size)
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
            info = os.fstat(fd)
            require(stat.S_ISREG(info.st_mode) and info.st_uid == 0
                    and not info.st_mode & 0o022 and info.st_size == (fitted or facts)["size_bytes"],
                    "staged image changed during source validation")
        # The validated image is still readable during apply and leaves no staging path
        # after WriteIssued, including if the installer is killed in the write loop.
        yield fd, facts, fitted
    finally:
        if fd is not None:
            os.close(fd)


def blocks(fd, size, *, start=0):
    offset = start
    while offset < size:
        buffer = os.pread(fd, min(BLOCK_SIZE, size - offset), offset)
        require(buffer, "unexpected end of device or image")
        yield offset, buffer
        offset += len(buffer)


def block_digest(fd, size):
    hasher = hashlib.sha256()
    for _, buffer in blocks(fd, size):
        hasher.update(buffer)
    return hasher.hexdigest()


def blank_digest(fd, size):
    hasher = hashlib.sha256()
    for offset, buffer in blocks(fd, size):
        require(buffer == ZERO[:len(buffer)], f"target is not blank at block offset {offset}")
        hasher.update(buffer)
    return hasher.hexdigest()


def write_blocks(source, target, start, end):
    for offset, block in blocks(source, end, start=start):
        remaining = memoryview(block)
        while remaining:
            written = os.pwrite(target, remaining, offset)
            require(written > 0, "device write made no progress")
            offset += written
            remaining = remaining[written:]


def plan(directory, args):
    require(not os.listdir(directory), "use a new empty state directory to plan an installation")
    receipt_sha256 = checked_digest(args.receipt_sha256)
    image = args.source_image.absolute()
    receipt = args.receipt.absolute()
    with open_target(args.target, write=False) as fd:
        target = read_target(fd, args.target, allow_loop=args.allow_loop)
        require_unallocated(fd, args.target)
        fit_size = target["size_bytes"] if args.allow_larger_target else None
        with staged_source(directory, image, receipt, receipt_sha256, fit_size=fit_size) as (_, source, fitted):
            require(target["size_bytes"] == (fitted or source)["size_bytes"],
                    "block device and native image must have the same exact size")
            require_unallocated(fd, args.target)
            original = blank_digest(fd, target["size_bytes"])
            require(read_target(fd, args.target, allow_loop=args.allow_loop) == target,
                    "block device changed while planning")
            require_unallocated(fd, args.target)
    request = {
        "id": str(uuid.uuid4()),
        "action": "FreshNativeInstall",
        "source_image": str(image),
        "receipt": str(receipt),
        "source": source,
        "target": target,
        "allow_loop": args.allow_loop,
        "blank_sha256": original,
        "encryption": "None",
        "retained_target_data": "None",
    }
    if fitted is not None:
        request["deployment_image"] = fitted
    entry = {
        "format": 1,
        "status": "Planned",
        "planned_at": timestamp(),
        "plan_digest": digest(request),
        "plan": request,
    }
    save_operation(directory, entry)
    return entry


def exact_target(fd, request):
    path = Path(request["target"]["path"])
    observed = read_target(fd, path, allow_loop=request["allow_loop"])
    require(observed == request["target"], "target disk identity or generation differs from plan")
    return path


def verified(directory, entry, *, reconciled, reconciled_target=None):
    settled = {
        **entry,
        "status": "Verified",
        "verified_at": timestamp(),
        "image_readback_sha256": expected_image(entry["plan"])["image_sha256"],
        "reconciled": reconciled,
    }
    if reconciled_target is not None:
        settled["reconciled_target"] = reconciled_target
    save_operation(directory, settled)
    return settled


def apply(directory, args):
    entry = read_operation(directory)
    require(checked_digest(args.plan_digest) == entry["plan_digest"],
            "approval does not match the exact installation plan")
    if entry["status"] == "Verified":
        return entry
    require(entry["status"] == "Planned", "disk writes were already issued; use inspect")
    request = entry["plan"]
    source = request["source"]
    target = Path(request["target"]["path"])
    deployment = request.get("deployment_image")
    installed = expected_image(request)
    size = installed["size_bytes"]
    with staged_source(directory, request["source_image"], request["receipt"],
                       source["receipt_sha256"], fit_size=deployment["size_bytes"] if deployment else None) as (image, facts, fitted):
        require(facts == source, "source image receipt and contents differ from plan")
        require(fitted == deployment, "target-sized installation image differs from plan")
        partitions = facts["partitions"]
        first_partition = min(part["start_sector"] for part in partitions) * 512
        last_partition_end = max(part["start_sector"] + part["sector_count"] for part in partitions) * 512
        require(0 < first_partition < last_partition_end < size,
                "verified image has no separate GPT publication ranges")
        with open_target(target, write=True) as fd:
            exact_target(fd, request)
            require_unallocated(fd, target)
            require(blank_digest(fd, size) == request["blank_sha256"], "target bytes changed after planning")
            exact_target(fd, request)
            require_unallocated(fd, target)
            issued = {**entry, "status": "WriteIssued", "write_issued_at": timestamp()}
            save_operation(directory, issued)
            exact_target(fd, request)
            require_unallocated(fd, target)
            # Flush all partition data before publishing either GPT copy.
            write_blocks(image, fd, first_partition, last_partition_end)
            os.fsync(fd)
            exact_target(fd, request)
            require_unallocated(fd, target)
            # The unused tail was verified blank. Only write the relocated backup GPT there.
            factory_size = source["size_bytes"]
            write_blocks(image, fd, last_partition_end, factory_size)
            if deployment is not None:
                write_blocks(image, fd, max(factory_size, deployment["backup_gpt_offset"]), size)
                os.fsync(fd)
            write_blocks(image, fd, 0, first_partition)
            os.fsync(fd)
            observed = block_digest(fd, size)
            exact_target(fd, request)
            require(observed == installed["image_sha256"], "installed disk does not match the verified image")
            return verified(directory, issued, reconciled=False)


def inspect(directory):
    entry = read_operation(directory)
    if entry["status"] != "WriteIssued":
        return entry
    request = entry["plan"]
    target = Path(request["target"]["path"])
    installed = expected_image(request)
    try:
        with open_target(target, write=False) as fd:
            exact_target(fd, request)
            observed = block_digest(fd, installed["size_bytes"])
            exact_target(fd, request)
            require(observed == installed["image_sha256"],
                    "device bytes do not match the verified image")
            os.fsync(fd)
            exact_target(fd, request)
            return verified(directory, entry, reconciled=True)
    except (OSError, ValueError) as error:
        return {**entry, "status": "RecoveryRequired", "reason": str(error)}


def reconcile_after_reboot(directory, args):
    entry = read_operation(directory)
    require(checked_digest(args.plan_digest) == entry["plan_digest"],
            "approval does not match the exact installation plan")
    if "reconciled_target" in entry:
        require(str(args.target) == entry["reconciled_target"]["path"],
                "target path does not match the recorded cross-boot result")
        return entry
    require(entry["status"] == "WriteIssued", "cross-boot reconciliation requires a previously issued write")
    request = entry["plan"]
    installed = expected_image(request)
    try:
        with open_target(args.target, write=False) as fd:
            observed = read_target(fd, args.target, allow_loop=False)
            require_reboot_target(request["target"], observed)
            require(block_digest(fd, installed["size_bytes"]) == installed["image_sha256"],
                    "device bytes do not match the verified image")
            require(read_target(fd, args.target, allow_loop=False) == observed,
                    "block device changed during cross-boot verification")
            return verified(directory, entry, reconciled=True, reconciled_target=observed)
    except (OSError, ValueError) as error:
        return {**entry, "status": "RecoveryRequired", "reason": str(error)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("plan", help="verify the image and an empty whole disk")
    prepare.add_argument("--source-image", type=Path, required=True)
    prepare.add_argument("--receipt", type=Path, required=True)
    prepare.add_argument("--receipt-sha256", required=True)
    prepare.add_argument("--target", type=Path, required=True)
    prepare.add_argument("--allow-larger-target", action="store_true",
                         help="relocate GPT backup metadata and leave additional capacity unallocated")
    prepare.add_argument("--allow-loop", action="store_true", help="allow an isolated loop test disk")
    execute = commands.add_parser("apply", help="approve and install an exact saved plan")
    execute.add_argument("--plan-digest", required=True)
    read = commands.add_parser("inspect", help="show the outcome and reconcile a prior write")
    reconcile = commands.add_parser("reconcile-after-reboot", help="read back a prior SATA write after Linux restarts")
    reconcile.add_argument("--plan-digest", required=True)
    reconcile.add_argument("--target", type=Path, required=True)
    for command in (prepare, execute, read, reconcile):
        command.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(os.geteuid() == 0 and sys.platform == "linux" and platform.machine() == "x86_64",
                "the native disk installer requires local root on Linux amd64")
        with lock_directory(args.state_dir) as directory:
            if args.command == "plan":
                result = plan(directory, args)
            elif args.command == "apply":
                result = apply(directory, args)
            elif args.command == "inspect":
                result = inspect(directory)
            else:
                result = reconcile_after_reboot(directory, args)
        print(json.dumps(result))
        return 1 if result["status"] == "RecoveryRequired" else 0
    except (OSError, ValueError) as error:
        parser.exit(1, f"installation refused: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
