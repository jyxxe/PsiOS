"""Private material and immutable evidence for managed native selection."""

from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
from pathlib import Path
import stat
import uuid

from activate_update import (
    CODE, DISK, FIRMWARE_LIMIT, JOURNAL, VARS, bytes_at, digest_json, file_sha256, read_operation, stage_contract,
)
from activation_guest import MAX_DISK_BYTES, artifact_digests, bound_artifacts, open_file, unique_fields
from create import MIB
from install import JOURNAL_LIMIT, checked_digest, encode, require


NAMES = (DISK, CODE, VARS)
HASHES = ("image_sha256", "efi_code_sha256", "efi_vars_sha256")
BUILD_RECEIPT = "candidate-build.json"


def fd_path(directory):
    return Path(f"/proc/{os.getpid()}/fd/{directory}")


def read_json(directory, name):
    with open_file(directory, name, writable=False) as (fd, size):
        require(size <= JOURNAL_LIMIT, "selection record exceeds its size limit")
        initial = bound_artifacts(directory, [name], [fd])
        with os.fdopen(os.dup(fd), "rb") as source:
            raw = source.read(JOURNAL_LIMIT + 1)
        require(len(raw) == size and bound_artifacts(directory, [name], [fd]) == initial,
                "selection record changed while being read")
    value = json.loads(raw, object_pairs_hook=unique_fields)
    require(isinstance(value, dict), "selection record must be an object")
    return value, raw


def save_json(directory, name, value):
    save_bytes(directory, name, encode(value) + b"\n")


def save_bytes(directory, name, raw):
    require(len(raw) <= JOURNAL_LIMIT, "selection record exceeds its size limit")
    temporary = f".selection-{uuid.uuid4().hex}"
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                 0o600, dir_fd=directory)
    try:
        with os.fdopen(fd, "wb") as target:
            os.fchmod(target.fileno(), 0o600)
            target.write(raw)
            target.flush()
            os.fsync(target.fileno())
        os.replace(temporary, name, src_dir_fd=directory, dst_dir_fd=directory)
        os.fsync(directory)
    finally:
        try:
            os.unlink(temporary, dir_fd=directory)
        except FileNotFoundError:
            pass


@contextmanager
def generation_directory(parent, name):
    fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
    try:
        info = os.fstat(fd)
        require(info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o700,
                "selected generation directory must be private and operator-owned")
        yield fd
    finally:
        os.close(fd)


def check_proof(deployment, current, proof):
    require(isinstance(current, dict) and set(current) == {*HASHES, "boot_id"}
            and isinstance(proof, dict), "invalid selected boot evidence")
    for key in HASHES:
        checked_digest(current[key])
    boot_id = current["boot_id"]
    signing = deployment["secure_boot"]
    expected_proof = {
        "boot_id": boot_id,
        "node_id": deployment["node_id"],
        "deployment_manifest_sha256": deployment["deployment_manifest_sha256"],
    }
    if signing is not None:
        require(isinstance(signing, dict) and "status" not in signing, "invalid Secure Boot evidence")
        expected_proof["secure_boot"] = {"status": "Enforced", **signing}
    require(isinstance(boot_id, str) and str(uuid.UUID(boot_id)) == boot_id and uuid.UUID(boot_id).int != 0
            and current["efi_code_sha256"] == deployment["firmware_code_sha256"]
            and proof == expected_proof, "selected boot evidence differs from deployment")


def check_material(directory, deployment, current):
    with ExitStack() as files:
        descriptors, sizes = zip(*(files.enter_context(open_file(directory, name, writable=False))
                                  for name in NAMES), strict=True)
        require(type(deployment["size_bytes"]) is int and 0 < deployment["size_bytes"] <= MAX_DISK_BYTES
                and sizes[0] == deployment["size_bytes"] and not sizes[0] % 512
                and sizes[1] + sizes[2] == 4 * MIB,
                "selected deployment has unexpected disk or firmware size")
        before = bound_artifacts(directory, NAMES, descriptors)
        observed = artifact_digests(descriptors, sizes)
        require(observed == {key: current[key] for key in HASHES}
                and bound_artifacts(directory, NAMES, descriptors) == before,
                "selected deployment disk or firmware bytes changed")


def check_build_receipt(directory, deployment):
    _, raw = read_json(directory, BUILD_RECEIPT)
    require(hashlib.sha256(raw).hexdigest() == deployment["candidate_receipt_sha256"],
            "selected build receipt changed")


def copy_build_receipt(directory, deployment):
    raw = bytes_at(Path(deployment["stage_dir"]) / BUILD_RECEIPT, JOURNAL_LIMIT)
    require(hashlib.sha256(raw).hexdigest() == deployment["candidate_receipt_sha256"],
            "staged build receipt changed during adoption")
    save_bytes(directory, BUILD_RECEIPT, raw)


def prepare_activation(directory, activation_dir, receipt_digest):
    entry, raw = read_json(directory, JOURNAL)
    require(hashlib.sha256(raw).hexdigest() == receipt_digest,
            "activation receipt SHA-256 differs from approval")
    require(entry == read_operation(directory) and entry["status"] == "BootVerified",
            "managed selection requires a complete BootVerified receipt")
    activation = entry["plan"]
    stage = Path(activation["stage_dir"])
    accepted = stage_contract(stage, checked_digest(activation["stage_receipt_sha256"]))
    require(all(activation.get(key) == value for key, value in accepted.items()),
            "activation no longer matches the approved staged update")
    size = accepted["size_bytes"]
    require(type(size) is int and 0 < size <= MAX_DISK_BYTES
            and file_sha256(stage / "candidate.raw", size) == accepted["candidate_sha256"],
            "staged candidate changed after activation")
    source = Path(activation["source_disk"])
    require(source.is_absolute() and ".." not in source.parts, "activation source disk is not an exact absolute path")
    staged = digest_json(stage / "staged-update.json", accepted["stage_receipt_sha256"])
    deployment = {
        **accepted,
        "activation_dir": str(activation_dir),
        "activation_receipt_sha256": receipt_digest,
        "activation_plan_digest": entry["plan_digest"],
        "activation_verified_at": entry["boot_verified_at"],
        "source_disk": str(source),
        "source_identity": activation["source_identity"],
        "source_previous_receipt_sha256": checked_digest(staged["source"]["previous_receipt_sha256"]),
        "stage_dir": str(stage),
    }
    proof = entry["proof"]
    current = {key: entry["managed"][key] for key in HASHES}
    current["boot_id"] = proof["boot_id"]
    check_proof(deployment, current, proof)
    check_material(directory, deployment, current)
    return deployment, current, proof


def copy_material(source, target, current):
    # Preserve sparse holes without accepting input-supplied pathnames or data
    # outside the approved immutable disk and firmware pair.
    zero = bytes(MIB)
    with ExitStack() as files:
        descriptors, sizes = zip(*(files.enter_context(open_file(source, name, writable=False))
                                  for name in NAMES), strict=True)
        before = bound_artifacts(source, NAMES, descriptors)
        for name, key, descriptor, size in zip(NAMES, HASHES, descriptors, sizes, strict=True):
            maximum = MAX_DISK_BYTES if name == DISK else FIRMWARE_LIMIT
            require(0 < size <= maximum, "selected disk or firmware exceeds its copy limit")
            fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                         0o600, dir_fd=target)
            digest = hashlib.sha256()
            offset = 0
            with os.fdopen(fd, "wb") as output:
                os.fchmod(output.fileno(), 0o600)
                while offset < size:
                    block = os.pread(descriptor, min(MIB, size - offset), offset)
                    require(block, "activation material shrank during copy")
                    digest.update(block)
                    if block == zero[:len(block)]:
                        output.seek(len(block), os.SEEK_CUR)
                    else:
                        output.write(block)
                    offset += len(block)
                output.truncate(size)
                output.flush()
                os.fsync(output.fileno())
            require(digest.hexdigest() == current[key], "activation material changed during copy")
        require(bound_artifacts(source, NAMES, descriptors) == before,
                "activation material identity changed during copy")
