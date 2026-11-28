#!/usr/bin/env python3
"""Stage a native PsiOS update after verifying its isolated UEFI trial."""

import argparse
from contextlib import contextmanager
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import subprocess
import sys
from tempfile import TemporaryDirectory

from create import boot_media, build_update_pair, copy_budget_mib, sha256
from update_trial import prove_trial


MIB = 1024 * 1024
RENAME_NOREPLACE = 1


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest_argument(value):
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise argparse.ArgumentTypeError("expected a lowercase SHA-256 digest")
    return value


def identity(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


@contextmanager
def publication_parent(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        info = os.fstat(fd)
        require(info.st_uid == os.geteuid() and not info.st_mode & 0o022,
                "destination parent must be owned by the operator and protected from other writers")
        yield fd
    finally:
        os.close(fd)


def open_cold_raw(source):
    fd = source.fileno()
    options = f"driver=raw,file.driver=file,file.filename=/proc/self/fd/{fd},file.locking=on"
    # qemu-img info permits a live writer. A comparison requests consistent reads.
    result = subprocess.run(["qemu-img", "compare", "-q", "--image-opts", options, options], pass_fds=(fd,),
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE, timeout=120)
    require(result.returncode == 0, "source disk is busy or QEMU could not acquire its read lock")


def require_unchanged(path, source, expected_identity, expected_sha256, description):
    source.seek(0)
    require(hashlib.file_digest(source, "sha256").hexdigest() == expected_sha256
            and identity(os.fstat(source.fileno())) == expected_identity
            and identity(path.stat(follow_symlinks=False)) == expected_identity,
            f"{description} changed before candidate publication")


def write_receipt(directory, name, fields):
    path = directory / name
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600)
    with os.fdopen(fd, "w", encoding="ascii") as stream:
        json.dump(fields, stream, sort_keys=True, separators=(",", ":"))
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    return sha256(path)


def rename_noreplace(source_fd, source, target_fd, target):
    rename = ctypes.CDLL(None, use_errno=True).renameat2
    rename.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    rename.restype = ctypes.c_int
    if rename(source_fd, os.fsencode(source), target_fd, os.fsencode(target), RENAME_NOREPLACE):
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), target)


def publish_bundle(bundle, parent_fd, basename):
    # Both directory descriptors refer to the same filesystem. The final name
    # appears atomically and an existing destination remains operator-owned.
    directory = os.open(bundle.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    payload = os.open(bundle, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(payload)
        rename_noreplace(directory, bundle.name, parent_fd, basename)
        try:
            os.fsync(parent_fd)
            os.fsync(directory)
        except OSError as error:
            raise ValueError(f"complete candidate directory is visible but its publication sync is unconfirmed: {basename}") from error
    finally:
        os.close(payload)
        os.close(directory)


def stage(args):
    output = args.output.absolute()
    require(output.name and ".." not in output.parts, "invalid output directory name")
    with publication_parent(output.parent) as parent_fd:
        parent = Path(f"/proc/{os.getpid()}/fd/{parent_fd}")
        require(not os.path.lexists(parent / output.name), "update output already exists")
        with TemporaryDirectory(dir=parent, prefix=".psios-update-") as scratch:
            work = Path(scratch)
            inputs = work / "inputs"
            inputs.mkdir(mode=0o700)
            prior = inputs / "previous-build.json"
            code = inputs / "ovmf-code.fd"
            variables = inputs / "ovmf-vars.fd"
            boot_media.pinned_copy(args.previous_receipt, prior, args.previous_receipt_sha256, 16 * 1024)
            boot_media.pinned_copy(args.ovmf_code, code, args.ovmf_code_sha256, 16 * MIB)
            boot_media.pinned_copy(args.ovmf_vars, variables, args.ovmf_vars_sha256, 16 * MIB)
            with boot_media.regular_file(args.upgrade_from) as old, boot_media.regular_file(args.ovmf_vars) as old_vars:
                old_identity = identity(os.fstat(old.fileno()))
                vars_identity = identity(os.fstat(old_vars.fileno()))
                open_cold_raw(old)

                prepare = work / "build"
                prepare.mkdir(mode=0o700)
                build_args = argparse.Namespace(**vars(args))
                build_args.previous_receipt = prior
                prepared = build_update_pair(prepare, build_args, old.fileno())
                trial_work = work / "trial"
                trial_work.mkdir(mode=0o700)
                proof = prove_trial(trial_work, prepared, code, variables)

                receipt = dict(prepared.candidate_receipt)
                receipt["image"] = str(output / "candidate.raw")
                original = receipt["upgrade"]["previous_image_sha256"]
                authority = receipt["upgrade"]["verified_authority"]
                candidate = prepared.candidate_disk
                require(sha256(candidate) == receipt["initial_image_sha256"],
                        "candidate changed during its independent trial")
                bundle = work / "bundle"
                bundle.mkdir(mode=0o700)
                os.link(candidate, bundle / "candidate.raw", follow_symlinks=False)
                receipt_sha256 = write_receipt(bundle, "candidate-build.json", receipt)
                record = {
                    "format": 1,
                    "status": "Staged",
                    "rollback_mode": "ManualRecoveryOnly",
                    "source": {
                        "image_sha256": original,
                        "normalized_image_sha256": prepared.normalized_source_sha256,
                        "previous_receipt_sha256": args.previous_receipt_sha256,
                        "node_id": authority["node_id"],
                        "database_format": authority["database_format"],
                    },
                    "inputs": {
                        "core_archive_sha256": args.core_sha256,
                        "max_copy_mib": args.max_copy_mib,
                        "firmware_code_sha256": args.ovmf_code_sha256,
                        "firmware_vars_sha256": args.ovmf_vars_sha256,
                    },
                    "candidate": {
                        "image_sha256": receipt["initial_image_sha256"],
                        "build_receipt_sha256": receipt_sha256,
                        "deployment_manifest_sha256": receipt["deployment_manifest_sha256"],
                        "source_commit": receipt["source_commit"],
                    },
                    "trial": {
                        **proof,
                        "image_sha256": prepared.trial_receipt["initial_image_sha256"],
                        "deployment_manifest_sha256": prepared.trial_receipt["deployment_manifest_sha256"],
                        "normalized_source_sha256": prepared.normalized_source_sha256,
                    },
                }
                if "secure_boot" in receipt:
                    record["candidate"]["secure_boot"] = receipt["secure_boot"]
                    record["candidate"]["secure_boot_enforcement_required"] = receipt["secure_boot_enforcement_required"]
                write_receipt(bundle, "staged-update.json", record)
                require_unchanged(args.upgrade_from, old, old_identity, original, "source disk")
                require_unchanged(args.ovmf_vars, old_vars, vars_identity, args.ovmf_vars_sha256, "source firmware variables")
                open_cold_raw(old)
                prepared.boot_tools.receipt()
                publish_bundle(bundle, parent_fd, output.name)
                return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    for name in ("upgrade-from", "previous-receipt", "kernel-iso", "core-archive", "ovmf-code", "ovmf-vars"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("previous-receipt-sha256", "core-sha256", "ovmf-code-sha256", "ovmf-vars-sha256"):
        parser.add_argument("--" + name, type=digest_argument, required=True)
    parser.add_argument("--max-copy-mib", type=copy_budget_mib, default=128,
                        help="authority database and journals to copy per verification (default 128 MiB)")
    boot_media.add_signing_arguments(parser)
    args = parser.parse_args()
    boot_media.signing_requested(args, parser)
    try:
        require(sys.platform == "linux" and platform.machine() == "x86_64",
                "isolated native staging requires Linux amd64")
        print(json.dumps(stage(args)))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"update staging refused: {error}\n")


if __name__ == "__main__":
    main()
