#!/usr/bin/env python3
"""Verify the first managed boot of a staged native PsiOS update."""

import argparse
from contextlib import ExitStack, contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import struct
import subprocess
import sys
import uuid

from activation_guest import artifact_digest, boot_candidate, bound_artifacts, open_file
from activation_storage import BUILD_RECEIPT, BootStorage
from create import MIB, boot_media, read_receipt
from install import (
    JOURNAL, JOURNAL_LIMIT, checked_digest, digest, encode, lock_directory, require, save_operation,
    timestamp, valid_instant,
)
from install_target import filesystem_type
from stage_update import identity, rename_noreplace, require_unchanged


DISK = "managed.raw"
CODE = "managed-ovmf-code.fd"
VARS = "managed-efi-vars.fd"
FIRMWARE_LIMIT = 16 * MIB
PREPARATION = "preparing.json"


def bytes_at(path, limit):
    with boot_media.regular_file(path) as stream:
        contents = stream.read(limit + 1)
    require(len(contents) <= limit, "activation input exceeds its size limit")
    return contents


def file_sha256(path, limit, *, flush=False):
    with boot_media.regular_file(path) as stream:
        before = identity(os.fstat(stream.fileno()))
        require(0 < before[2] <= limit, "activation file has an invalid size")
        if flush:
            os.fsync(stream.fileno())
        value = hashlib.file_digest(stream, "sha256").hexdigest()
        require(identity(os.fstat(stream.fileno())) == before, "activation file changed while being verified")
    return value


def verify_stopped_source(path, expected):
    require(identity(path.stat(follow_symlinks=False)) == tuple(expected),
            "stopped source disk identity changed during the operation")


@contextmanager
def stopped_source(path, expected, size, previous_identity=None):
    # QEMU uses Linux byte-range locks for its file block driver. A whole-file
    # OFD write lock conflicts with these locks without relying on QEMU's offsets.
    # No source descriptor is inherited by the candidate QEMU process.
    listed = path.stat(follow_symlinks=False)
    require(stat.S_ISREG(listed.st_mode), "stopped source must be a regular local image")
    fd = os.open(path, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(fd)
        observed = identity(info)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid() and not info.st_mode & 0o022
                and info.st_nlink == 1 and filesystem_type(fd) in (b"ext4", b"xfs", b"btrfs"),
                "stopped source must be a private, operator-owned local image")
        require(observed == identity(listed) and observed[2] == size
                and (previous_identity is None or tuple(previous_identity) == observed),
                "original disk identity or size changed after planning")
        # Linux amd64 struct flock with whence=SEEK_SET, start=0 and len=0
        # locks the entire file, including any future extension.
        try:
            fcntl.fcntl(fd, fcntl.F_OFD_SETLK, struct.pack("@hhqqi4x", fcntl.F_WRLCK, os.SEEK_SET, 0, 0, 0))
        except OSError as error:
            raise ValueError("source disk is busy or the local filesystem refused its exclusive lock") from error
        require_unchanged(path, source, observed, expected, "original disk")
        yield list(observed)


def digest_json(path, expected):
    contents = bytes_at(path, JOURNAL_LIMIT)
    require(hashlib.sha256(contents).hexdigest() == expected, "staged update receipt SHA-256 mismatch")
    value = json.loads(contents)
    require(isinstance(value, dict), "staged update receipt is invalid")
    return value


def stage_contract(bundle, stage_sha256):
    stage = digest_json(bundle / "staged-update.json", stage_sha256)
    require(stage.get("format") == 1 and stage.get("status") == "Staged"
            and stage.get("rollback_mode") == "ManualRecoveryOnly",
            "operation requires a verified staged native update")
    try:
        source, inputs = stage["source"], stage["inputs"]
        candidate, trial = stage["candidate"], stage["trial"]
        authority = trial["verified_authority"]
        receipt_path = bundle / BUILD_RECEIPT
        receipt_sha256 = checked_digest(candidate["build_receipt_sha256"])
        receipt = digest_json(receipt_path, receipt_sha256)
        parsed, layout, _ = read_receipt(receipt_path)
        upgrade = receipt["upgrade"]
        source_authority = upgrade["verified_authority"]
        require(receipt == parsed
                and receipt["initial_image_sha256"] == candidate["image_sha256"]
                and receipt["deployment_manifest_sha256"] == candidate["deployment_manifest_sha256"]
                and receipt["source_commit"] == candidate["source_commit"]
                and upgrade["previous_image_sha256"] == source["image_sha256"]
                and upgrade["normalized_source_sha256"] == source["normalized_image_sha256"]
                and source_authority["node_id"] == source["node_id"] == authority["node_id"]
                and trial["status"] == "Verified" and trial["boot_mode"] == "IsolatedTrial"
                and trial["state_uuid"] == receipt["state_uuid"] and trial["home_uuid"] == receipt["home_uuid"]
                and trial["normalized_source_sha256"] == source["normalized_image_sha256"]
                and trial["firmware_code_sha256"] == inputs["firmware_code_sha256"]
                and trial["firmware_vars_sha256"] == inputs["firmware_vars_sha256"]
                and authority["witness"] == "Confirmed"
                and authority["sqlite_integrity"] == authority["foreign_keys"] == "Verified"
                and authority["migration_required"] is False
                and authority["database_format"] == source_authority["migration_rehearsal"]["target_format"],
                "candidate and isolated trial do not reference the same verified deployment")
        if any("max_copy_mib" in record for record in (inputs, source_authority, authority)):
            budget = inputs.get("max_copy_mib")
            require(type(budget) is int and 1 <= budget <= 256
                    and all(type(report.get("max_copy_mib")) is int and report["max_copy_mib"] == budget
                            and type(report.get("copied_bytes")) is int
                            and 0 < report["copied_bytes"] <= budget * MIB
                            for report in (source_authority, authority)),
                    "staged source and trial authority reports disagree with their copy budget")
        signing = receipt.get("secure_boot")
        if signing is None:
            require("secure_boot" not in candidate and "secure_boot" not in trial
                    and "secure_boot_enforcement_required" not in receipt
                    and "secure_boot_enforcement_required" not in candidate,
                    "unsigned candidate has inconsistent Secure Boot evidence")
        else:
            require(candidate.get("secure_boot") == signing
                    and trial.get("secure_boot") == {"status": "Enforced", **signing}
                    and receipt.get("secure_boot_enforcement_required") is True
                    and candidate.get("secure_boot_enforcement_required") is True,
                    "signed candidate requires pre-mount enforcement and matching isolated proof")
        require(isinstance(source["node_id"], str) and source["node_id"].startswith("node-"),
                "staged update has an invalid source authority")
        return {
            "stage_receipt_sha256": stage_sha256,
            "candidate_receipt_sha256": receipt_sha256,
            "candidate_sha256": checked_digest(candidate["image_sha256"]),
            "source_sha256": checked_digest(source["image_sha256"]),
            "deployment_manifest_sha256": checked_digest(candidate["deployment_manifest_sha256"]),
            "firmware_code_sha256": checked_digest(inputs["firmware_code_sha256"]),
            "firmware_vars_sha256": checked_digest(inputs["firmware_vars_sha256"]),
            "node_id": source["node_id"],
            "database_format": authority["database_format"],
            "size_bytes": layout.image_mib * MIB,
            "secure_boot": signing,
        }
    except (KeyError, TypeError, AttributeError) as error:
        raise ValueError("staged update lineage is incomplete") from error


def read_operation(directory, name=JOURNAL):
    path = Path(f"/proc/{os.getpid()}/fd/{directory}") / name
    with boot_media.regular_file(path) as stream:
        info = os.fstat(stream.fileno())
        require(info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1,
                "untrusted managed activation journal")
        raw = stream.read(JOURNAL_LIMIT + 1)
    require(len(raw) <= JOURNAL_LIMIT, "managed activation journal exceeds its size limit")
    entry = json.loads(raw)
    require(isinstance(entry, dict) and type(entry.get("format")) is int and entry["format"] == 1
            and entry.get("status") in ("BootPending", "BootIssued", "BootVerified")
            and valid_instant(entry.get("planned_at"))
            and entry.get("rollback_mode") == "ManualRecoveryOnly"
            and isinstance(entry.get("plan"), dict) and entry.get("plan_digest") == digest(entry["plan"])
            and entry["plan"].get("action") == "ManagedNativeFirstBoot",
            "managed activation journal is invalid")
    if entry["status"] != "BootPending":
        require(valid_instant(entry.get("boot_issued_at")), "activation lacks its boot witness")
    if entry["status"] == "BootVerified":
        proof = entry.get("proof")
        managed = entry.get("managed")
        require(valid_instant(entry.get("boot_verified_at")) and isinstance(managed, dict)
                and (managed.get("disk"), managed.get("efi_code"), managed.get("efi_vars")) == (DISK, CODE, VARS)
                and isinstance(proof, dict) and isinstance(proof.get("boot_id"), str)
                and str(uuid.UUID(proof["boot_id"])) == proof["boot_id"]
                and proof.get("node_id") == entry["plan"].get("node_id")
                and proof.get("deployment_manifest_sha256") == entry["plan"].get("deployment_manifest_sha256"),
                "activation lacks verified boot evidence")
        for key in ("image_sha256", "efi_code_sha256", "efi_vars_sha256"):
            checked_digest(managed.get(key))
        signing = entry["plan"].get("secure_boot")
        require(proof.get("secure_boot") == ({"status": "Enforced", **signing} if signing else None),
                "verified boot receipt has an inconsistent Secure Boot policy")
    return entry


@contextmanager
def anonymous_file(directory):
    fd = os.open(".", os.O_TMPFILE | os.O_RDWR | os.O_CLOEXEC, 0o600, dir_fd=directory)
    with os.fdopen(fd, "w+b") as stream:
        os.fchmod(fd, 0o600)
        yield stream


def publish_anonymous(stream, directory, name):
    os.link(f"/proc/self/fd/{stream.fileno()}", name, dst_dir_fd=directory, follow_symlinks=True)


def prepare(directory, args):
    names = set()
    with os.scandir(directory) as children:
        for child in children:
            require(child.name in (JOURNAL, PREPARATION, DISK, CODE, VARS),
                    "unrecognized contents in managed activation state directory")
            names.add(child.name)
    record = JOURNAL if JOURNAL in names else PREPARATION
    require(not names or (JOURNAL in names) != (PREPARATION in names),
            "expected one activation record for existing boot material")
    existing = read_operation(directory, record) if names else None
    require(existing is None or existing["status"] == "BootPending",
            "cannot prepare a plan after a boot has been issued")
    bundle = args.stage_dir.absolute()
    original = args.source_disk.absolute()
    accepted = stage_contract(bundle, checked_digest(args.stage_receipt_sha256))
    with stopped_source(original, accepted["source_sha256"], accepted["size_bytes"]) as frozen:
        facts = {**accepted, "action": "ManagedNativeFirstBoot", "stage_dir": str(bundle),
                 "source_disk": str(original), "source_identity": frozen}
        if existing is not None:
            require(isinstance(existing["plan"].get("id"), str)
                    and existing["plan"] == {**facts, "id": existing["plan"]["id"]},
                    "activation preparation belongs to a different plan")
        else:
            plan = {**facts, "id": str(uuid.uuid4())}
            existing = {"format": 1, "status": "BootPending", "rollback_mode": "ManualRecoveryOnly",
                        "planned_at": timestamp(), "plan_digest": digest(plan), "plan": plan}
        inputs = ((bundle / "candidate.raw", DISK, accepted["candidate_sha256"], accepted["size_bytes"]),
                  (args.ovmf_code, CODE, accepted["firmware_code_sha256"], FIRMWARE_LIMIT),
                  (args.ovmf_vars, VARS, accepted["firmware_vars_sha256"], FIRMWARE_LIMIT))
        materialize_plan(directory, names, inputs, existing)
        return existing


def materialize_plan(directory, names, inputs, entry):
    # Anonymous files disappear on process loss. Only fsynced, verified copies
    # can acquire a name, after the precise recovery record is durable.
    require(JOURNAL not in names or names == {JOURNAL, DISK, CODE, VARS},
            "published activation plan is missing private boot material")
    with ExitStack() as custody:
        copies = {}
        held = {}
        for source, name, expected, limit in inputs:
            if name in names:
                require(file_sha256(source, limit) == expected, "activation input differs from the recorded plan")
                fd, size = custody.enter_context(open_file(directory, name, writable=True))
                before = bound_artifacts(directory, [name], [fd])
                require(size <= limit and artifact_digest(fd, size) == expected
                        and bound_artifacts(directory, [name], [fd]) == before,
                        "published activation material differs from plan")
                held[name] = (fd, before)
            else:
                stream = custody.enter_context(anonymous_file(directory))
                with boot_media.regular_file(source) as src:
                    boot_media.copy_pinned(src, stream, source, expected, limit)
                stream.flush()
                fd = stream.fileno()
                os.fsync(fd)
                size = os.fstat(fd).st_size
                require(0 < size <= limit and artifact_digest(fd, size) == expected,
                        "staged activation copy changed")
                copies[name] = stream
        plan = entry["plan"]
        verify_stopped_source(Path(plan["source_disk"]), plan["source_identity"])
        if not names:
            raw = encode(entry) + b"\n"
            require(len(raw) <= JOURNAL_LIMIT, "activation plan exceeds its size limit")
            with anonymous_file(directory) as marker:
                marker.write(raw)
                marker.flush()
                os.fsync(marker.fileno())
                publish_anonymous(marker, directory, PREPARATION)
        # A previous process may have died just after linking the marker.
        if JOURNAL not in names:
            os.fsync(directory)
        for name, stream in copies.items():
            publish_anonymous(stream, directory, name)
        for name, (fd, before) in held.items():
            require(bound_artifacts(directory, [name], [fd]) == before,
                    "published activation material changed during preparation")
        for name, stream in copies.items():
            bound_artifacts(directory, [name], [stream.fileno()])
        verify_stopped_source(Path(plan["source_disk"]), plan["source_identity"])
        os.fsync(directory)
        if JOURNAL not in names:
            rename_noreplace(directory, PREPARATION, directory, JOURNAL)
            os.fsync(directory)


def secret_at(path):
    with boot_media.regular_file(path) as stream:
        meta = os.fstat(stream.fileno())
        require(meta.st_uid == os.geteuid() and not meta.st_mode & 0o077,
                "console credential file must be private and belong to the operator")
        secret = stream.read(130)
    secret = secret.removesuffix(b"\n")
    require(1 <= len(secret) <= 128 and all(32 <= byte <= 126 for byte in secret),
            "console credential must be a single bounded printable line")
    return secret


def activate(directory, work, args):
    entry = read_operation(directory)
    require(checked_digest(args.plan_digest) == entry["plan_digest"], "approval differs from the exact activation plan")
    if entry["status"] == "BootVerified":
        return entry
    require(entry["status"] == "BootPending", "a boot was already issued; inspect the existing operation")
    plan = entry["plan"]
    original, bundle = Path(plan["source_disk"]), Path(plan["stage_dir"])
    disk, code, variables = (work / name for name in (DISK, CODE, VARS))
    size = plan["size_bytes"]
    accepted = stage_contract(bundle, plan["stage_receipt_sha256"])
    require(all(plan.get(key) == value for key, value in accepted.items()), "staged update differs from plan")
    require(file_sha256(bundle / "candidate.raw", size) == plan["candidate_sha256"],
            "managed boot inputs changed after planning")
    secret = secret_at(args.root_console_password_file)

    with stopped_source(original, plan["source_sha256"], size, plan["source_identity"]) as frozen:
        return activate_frozen(directory, work, entry, original, frozen, (disk, code, variables), secret)


def activate_frozen(directory, work, entry, original, frozen, paths, secret):
    plan = entry["plan"]
    disk, code, variables = paths
    signing = plan.get("secure_boot")
    storage = BootStorage.from_receipt(Path(plan["stage_dir"]) / BUILD_RECEIPT,
                                        plan["candidate_receipt_sha256"], plan["size_bytes"])

    def issue():
        verify_stopped_source(original, frozen)
        entry.update(status="BootIssued", boot_issued_at=timestamp())
        save_operation(directory, entry)

    with boot_candidate(work, disk=disk, firmware_code=code, firmware_vars=variables,
                        credential=secret, expected_node=plan["node_id"],
                        manifest_sha256=plan["deployment_manifest_sha256"], before_launch=issue,
                        expected_artifacts={
                            "image_sha256": plan["candidate_sha256"],
                            "efi_code_sha256": plan["firmware_code_sha256"],
                            "efi_vars_sha256": plan["firmware_vars_sha256"],
                        }, storage=storage, secure_boot_required=signing is not None) as result:
        proof = result["proof"]
        require(entry["status"] == "BootIssued" and proof["node_id"] == plan["node_id"]
                and proof["deployment_manifest_sha256"] == plan["deployment_manifest_sha256"]
                and str(uuid.UUID(proof["boot_id"])) == proof["boot_id"]
                and proof.get("secure_boot") == ("Enforced" if signing else None),
                "managed launch did not prove the approved deployment")
        if signing:
            proof["secure_boot"] = {"status": "Enforced", **signing}
        verify_stopped_source(original, frozen)
        readback = {"disk": DISK, "efi_code": CODE, "efi_vars": VARS, **result["artifacts"]}
        entry.update(status="BootVerified", boot_verified_at=timestamp(), proof=proof, managed=readback)
        save_operation(directory, entry)
        return entry


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan")
    for name in ("stage-dir", "source-disk", "ovmf-code", "ovmf-vars"):
        plan.add_argument("--" + name, type=Path, required=True)
    plan.add_argument("--stage-receipt-sha256", required=True)
    run = commands.add_parser("activate")
    run.add_argument("--plan-digest", required=True)
    run.add_argument("--root-console-password-file", type=Path, required=True)
    read = commands.add_parser("inspect")
    for command in (plan, run, read):
        command.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(os.geteuid() != 0 and sys.platform == "linux" and platform.machine() == "x86_64",
                "managed boot verification requires an unprivileged local Linux amd64 operator")
        with lock_directory(args.state_dir, owner=os.geteuid()) as directory:
            work = Path(f"/proc/{os.getpid()}/fd/{directory}")
            if args.command == "plan":
                result = prepare(directory, args)
            elif os.path.lexists(work / PREPARATION):
                require(not os.path.lexists(work / JOURNAL), "conflicting activation records")
                require(args.command == "inspect", "activation preparation is incomplete; retry the original plan")
                entry = read_operation(directory, PREPARATION)
                require(entry["status"] == "BootPending", "invalid activation preparation")
                result = {**entry, "status": "Preparing"}
            elif args.command == "activate":
                result = activate(directory, work, args)
            else:
                result = read_operation(directory)
                if result["status"] == "BootIssued":
                    result = {**result, "status": "RecoveryRequired", "reason": "issued boot has no verified receipt"}
        print(json.dumps(result))
        return 1 if result["status"] == "RecoveryRequired" else 0
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"activation stopped: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
