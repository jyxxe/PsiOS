"""Durable custody of the one unpublished managed selection copy."""

from contextlib import ExitStack, contextmanager
import os
import re
import stat
import uuid

from activate_update import CODE, DISK, VARS
from activation_guest import MAX_DISK_BYTES, artifact_identity
from create import MIB
from install import JOURNAL_LIMIT, checked_digest, require
from select_manifest import BUILD_RECEIPT, fd_path, generation_directory, read_json, save_json


MARKER = "selection-copy.json"
NAME_PREFIX = ".selection-copy-"
FIELDS = {"format", "status", "plan_digest", "generation", "size_bytes", "name"}


def copy_status(directory):
    try:
        entry, _ = read_json(directory, MARKER)
    except FileNotFoundError:
        return None
    state = entry.get("status")
    keys = FIELDS | ({"device", "inode"} if state == "Copying" else set())
    require(set(entry) == keys and type(entry.get("format")) is int and entry["format"] == 1
            and state in ("Reserved", "Copying")
            and isinstance(entry.get("name"), str)
            and re.fullmatch(r"\.selection-copy-[0-9a-f]{32}", entry["name"])
            and type(entry.get("generation")) is int and 0 < entry["generation"] < 2**63
            and type(entry.get("size_bytes")) is int
            and 0 < entry["size_bytes"] <= MAX_DISK_BYTES and not entry["size_bytes"] % 512,
            "invalid managed selection copy marker")
    checked_digest(entry["plan_digest"])
    if state == "Copying":
        require(type(entry["device"]) is int and 0 <= entry["device"] < 2**64
                and type(entry["inode"]) is int and 0 < entry["inode"] < 2**64,
                "managed selection copy lacks a valid directory identity")
    return entry


def require_plan(directory, plan_digest):
    approved = checked_digest(plan_digest)
    entry = copy_status(directory)
    require(entry is None or entry["plan_digest"] == approved,
            "a staging copy belongs to a different adoption plan")
    return entry


def generation_facts(plan, generation_name=None):
    generation = plan["generation"]
    size = plan["deployment"]["size_bytes"]
    require(type(generation) is int and 0 < generation < 2**63
            and type(size) is int and 0 < size <= MAX_DISK_BYTES and not size % 512,
            "invalid adoption plan generation or disk size")
    if generation_name is not None:
        require(generation_name == f"generation-{generation}",
                "staging operation is outside the exact approved generation")
    return generation, size


def require_copy(directory, plan, approved, generation_name):
    generation, size = generation_facts(plan, generation_name)
    entry = require_plan(directory, approved)
    require(entry is None or (entry["generation"], entry["size_bytes"]) == (generation, size),
            "staging copy differs from the approved generation or disk size")
    return entry


def inspect_name(directory, name):
    try:
        return os.stat(name, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return None


def confirm_directory(directory, name, target, entry):
    listed = inspect_name(directory, name)
    held = os.fstat(target)
    require(listed is not None and os.path.samestat(listed, held)
            and held.st_dev == os.fstat(directory).st_dev
            and held.st_uid == os.geteuid() and stat.S_ISDIR(held.st_mode)
            and stat.S_IMODE(held.st_mode) == 0o700,
            "managed selection copy directory identity changed")
    if entry["status"] == "Copying":
        require((held.st_dev, held.st_ino) == (entry["device"], entry["inode"]),
                "managed selection copy is not the recorded directory inode")
    return held


@contextmanager
def staging(directory, plan, plan_digest):
    generation, size = generation_facts(plan)
    approved = checked_digest(plan_digest)
    require(require_plan(directory, approved) is None,
            "an unfinished staging copy requires exact discard before retry")
    require(inspect_name(directory, f"generation-{generation}") is None,
            "cannot allocate a staging copy for a published generation")
    name = NAME_PREFIX + uuid.uuid4().hex
    require(inspect_name(directory, name) is None, "new staging copy name is already occupied")
    entry = {"format": 1, "status": "Reserved", "plan_digest": approved,
             "generation": generation, "size_bytes": size, "name": name}
    save_json(directory, MARKER, entry)
    os.mkdir(name, 0o700, dir_fd=directory)
    with generation_directory(directory, name) as target:
        held = confirm_directory(directory, name, target, entry)
        os.fsync(target)
        os.fsync(directory)
        entry.update(status="Copying", device=held.st_dev, inode=held.st_ino)
        save_json(directory, MARKER, entry)
        yield fd_path(directory) / name, target


def verify_staging(directory, target, plan, plan_digest):
    entry = require_copy(directory, plan, plan_digest, None)
    require(entry is not None and entry["status"] == "Copying",
            "managed selection copy is not ready for publication")
    confirm_directory(directory, entry["name"], target, entry)


def clear_marker(directory, expected):
    require(copy_status(directory) == expected, "managed selection copy marker changed")
    os.unlink(MARKER, dir_fd=directory)
    os.fsync(directory)


def finish_publication(directory, plan, plan_digest, generation_name):
    entry = require_copy(directory, plan, plan_digest, generation_name)
    if entry is None:
        return
    require(entry["status"] == "Copying" and inspect_name(directory, entry["name"]) is None,
            "published selection still has an inconsistent staging name")
    with generation_directory(directory, generation_name) as published:
        confirm_directory(directory, generation_name, published, entry)
        os.fsync(published)
        os.fsync(directory)
        confirm_directory(directory, generation_name, published, entry)
        clear_marker(directory, entry)


def empty(directory):
    with os.scandir(directory) as children:
        return next(children, None) is None


def inspect_entries(directory, entry, custody):
    if entry["status"] == "Reserved":
        require(empty(directory), "reserved selection copy is not empty")
        return {}
    limits = {DISK: entry["size_bytes"], CODE: 4 * MIB, VARS: 4 * MIB,
              BUILD_RECEIPT: JOURNAL_LIMIT, "adoption.json": JOURNAL_LIMIT}
    if entry["generation"] > 1:
        limits["previous-selection.json"] = JOURNAL_LIMIT
    observed = {}
    with os.scandir(directory) as children:
        for child in children:
            name = child.name
            require(name in limits and name not in observed, "unrecognized entry in managed selection copy")
            fd = os.open(name, os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=directory)
            custody.callback(os.close, fd)
            info = os.fstat(fd)
            mode = stat.S_IMODE(info.st_mode)
            require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid() and info.st_nlink == 1
                    and 0 <= info.st_size <= limits[name]
                    and (mode == 0o600 or (info.st_size == 0 and not mode & ~0o600)),
                    "unsafe file in managed selection copy")
            observed[name] = artifact_identity(info)
    return observed


def discard(directory, plan, plan_digest, generation_name):
    entry = require_copy(directory, plan, plan_digest, generation_name)
    require(inspect_name(directory, generation_name) is None,
            "cannot discard staging for a published generation")
    generation, _ = generation_facts(plan)
    approved = checked_digest(plan_digest)
    if entry is None:
        return {"status": "AlreadyDiscarded", "generation": generation, "plan_digest": approved}
    name = entry["name"]
    if inspect_name(directory, name) is not None:
        with generation_directory(directory, name) as target, ExitStack() as custody:
            confirm_directory(directory, name, target, entry)
            files = inspect_entries(target, entry, custody)
            for child, previous in files.items():
                observed = inspect_name(target, child)
                require(observed is not None and artifact_identity(observed) == previous,
                        "managed selection copy changed during discard")
                os.unlink(child, dir_fd=target)
            require(empty(target), "managed selection copy is no longer empty")
            os.fsync(target)
            confirm_directory(directory, name, target, entry)
            os.rmdir(name, dir_fd=directory)
    os.fsync(directory)
    require(inspect_name(directory, generation_name) is None, "generation appeared during staging discard")
    clear_marker(directory, entry)
    return {"status": "Discarded", "generation": generation, "plan_digest": approved}
