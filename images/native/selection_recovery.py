"""Durable evidence for an already completed managed native boot."""

from contextlib import ExitStack, contextmanager
import hashlib
import os
import uuid

from activate_update import stopped_source
from activation_guest import MAX_DISK_BYTES, MIB, artifact_digest, bound_artifacts, open_file
from install import JOURNAL_LIMIT, checked_digest, encode, require, valid_instant
from select_manifest import HASHES, NAMES, check_proof, fd_path, read_json, save_bytes
from stage_update import identity


BOOT_PROOF = "boot-proof.json"
SETTLEMENT_FIELDS = {"status", "current", "proof", "boot_generation", "boot_verified_at", "selection_digest"}


def _record(issued, settled):
    return {"format": 1, "issued": issued, "settled": settled}


def _settlement_bytes(issued, settled):
    return encode(_record(issued, settled)) + b"\n"


def check_capacity(issued):
    """Prove the result can fit before launching a guest that can change the disk."""
    # The validated proof adds no variable-length data beyond its new timestamp.
    # Boot IDs and digests have fixed lengths and the deployment stays identical.
    settled = {**issued, "status": "Selected", "boot_generation": issued["boot_generation"] + 1,
               "boot_verified_at": issued["boot_issued_at"]}
    require(len(_settlement_bytes(issued, settled)) <= JOURNAL_LIMIT, "boot proof exceeds its size limit")


def _check_transition(issued, settled):
    require(isinstance(issued, dict) and isinstance(settled, dict)
            and issued.get("status") == "BootIssued" and settled.get("status") == "Selected"
            and valid_instant(issued.get("boot_issued_at")) and "boot_verified_at" not in issued
            and valid_instant(settled.get("boot_verified_at")),
            "boot proof does not contain an issued and settled boot")
    attempt = issued.get("boot_attempt_id")
    require(isinstance(attempt, str), "boot proof lacks a boot attempt ID")
    parsed_attempt = uuid.UUID(attempt)
    require(str(parsed_attempt) == attempt and parsed_attempt.int != 0, "boot proof has an invalid boot attempt ID")
    previous_boot = issued.get("boot_generation")
    next_boot = settled.get("boot_generation")
    require(type(previous_boot) is int and type(next_boot) is int
            and 0 <= previous_boot < next_boot < 2**63 and next_boot == previous_boot + 1,
            "boot proof has a nonconsecutive boot generation")
    issued_lineage = {key: value for key, value in issued.items() if key not in SETTLEMENT_FIELDS}
    settled_lineage = {key: value for key, value in settled.items() if key not in SETTLEMENT_FIELDS}
    require(encode(issued_lineage) == encode(settled_lineage), "boot proof changed the issued attempt or selection lineage")
    check_proof(issued["deployment"], issued["current"], issued["proof"])
    check_proof(settled["deployment"], settled["current"], settled["proof"])
    require(issued["current"]["boot_id"] != settled["current"]["boot_id"]
            and checked_digest(issued["selection_digest"]) != checked_digest(settled["selection_digest"]),
            "boot proof did not produce a fresh boot or selection digest")


def seal(directory, issued, updated):
    """Publish the exact checked transition and return the digest of its stored bytes."""
    _check_transition(issued, updated)
    raw = _settlement_bytes(issued, updated)
    save_bytes(directory, BOOT_PROOF, raw)
    return hashlib.sha256(raw).hexdigest()


def read(directory, validate_selection):
    """Require valid canonical proof bytes and exact lineage for reconciliation."""
    record, raw = read_json(directory, BOOT_PROOF)
    require(set(record) == {"format", "issued", "settled"} and type(record["format"]) is int
            and record["format"] == 1 and raw == encode(record) + b"\n", "invalid boot proof record")
    issued, settled = record["issued"], record["settled"]
    _check_transition(issued, settled)
    validate_selection(issued)
    validate_selection(settled)
    return record, hashlib.sha256(raw).hexdigest()


@contextmanager
def final_artifacts(directory, updated):
    """Check the named post-boot artifacts and retain their locks through publication."""
    deployment, current = updated["deployment"], updated["current"]
    with ExitStack() as custody:
        descriptors, sizes = zip(*(custody.enter_context(open_file(directory, name, writable=index != 1))
                                  for index, name in enumerate(NAMES)), strict=True)
        require(type(deployment["size_bytes"]) is int and 0 < sizes[0] <= MAX_DISK_BYTES
                and sizes[0] == deployment["size_bytes"] and sizes[0] % 512 == 0
                and sizes[1] + sizes[2] == 4 * MIB, "invalid boot proof artifact geometry")
        before = bound_artifacts(directory, NAMES, descriptors)
        for index in (0, 2):
            descriptor, size = descriptors[index], sizes[index]
            custody.enter_context(stopped_source(fd_path(directory) / NAMES[index], current[HASHES[index]],
                                                 size, identity(os.fstat(descriptor))))
        require(artifact_digest(descriptors[1], sizes[1]) == current[HASHES[1]],
                "boot proof firmware code changed")

        def verify():
            require(bound_artifacts(directory, NAMES, descriptors) == before,
                    "boot proof artifact identity changed during reconciliation")

        verify()
        yield verify
