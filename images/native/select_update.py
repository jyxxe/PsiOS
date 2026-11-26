#!/usr/bin/env python3
"""Select and boot an exact, verified native PsiOS deployment within one managed node."""

import argparse
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import uuid

import selection_recovery
import selection_scratch
from activate_update import DISK, VARS, stopped_source, verify_stopped_source
from activation_guest import open_file
from install import checked_digest, digest, lock_directory, require, timestamp, valid_instant
from select_manifest import (
    check_build_receipt, check_material, check_proof, copy_build_receipt, copy_material, fd_path, generation_directory,
    prepare_activation, read_json, save_json, write_json_exclusive,
)
from selection_boot import boot
from stage_update import identity, publish_bundle


SELECTION = "selection.json"
PLAN = "selection-plan.json"
ADOPTION = "adoption.json"
PREVIOUS = "previous-selection.json"
ROLLBACK = "ManualRecoveryOnly"
SELECTION_FIELDS = ("generation", "boot_generation", "adoption_plan_digest", "previous_selection_digest",
                    "deployment", "current", "proof")


def generation_name(generation):
    require(type(generation) is int and 0 < generation < 2**63, "invalid selected generation")
    return f"generation-{generation}"


def selection_digest(entry):
    return digest(["psios-native-selection-v1", {name: entry[name] for name in SELECTION_FIELDS}])


def plan_digest(plan):
    return digest(["psios-native-adoption-v1", plan])


def optional_json(directory, name):
    try:
        return read_json(directory, name)[0]
    except FileNotFoundError:
        return None


def read_plan(directory):
    entry = optional_json(directory, PLAN)
    if entry is None:
        return None
    plan = entry.get("plan")
    require(type(entry.get("format")) is int and entry["format"] == 1
            and entry.get("status") == "Planned" and valid_instant(entry.get("planned_at"))
            and isinstance(plan, dict) and plan_digest(plan) == entry.get("plan_digest")
            and plan["generation"] == plan["expected_generation"] + 1,
            "managed adoption plan is invalid")
    generation_name(plan["generation"])
    return entry


def validate_selection(entry):
    require(type(entry.get("format")) is int and entry["format"] == 1
            and entry.get("status") in ("Selected", "BootIssued")
            and entry.get("rollback_mode") == ROLLBACK
            and valid_instant(entry.get("selected_at"))
            and type(entry.get("boot_generation")) is int and 0 <= entry["boot_generation"] < 2**63
            and entry.get("selection_digest") == selection_digest(entry),
            "managed selection record is invalid")
    generation_name(entry["generation"])
    checked_digest(entry["adoption_plan_digest"])
    if entry["generation"] == 1:
        require(entry["previous_selection_digest"] is None, "initial selection has an unexpected predecessor")
    else:
        checked_digest(entry["previous_selection_digest"])
    check_proof(entry["deployment"], entry["current"], entry["proof"])
    if "boot_attempt_id" in entry:
        attempt = entry["boot_attempt_id"]
        require(isinstance(attempt, str) and str(uuid.UUID(attempt)) == attempt and uuid.UUID(attempt).int != 0,
                "managed selection has an invalid boot attempt ID")
    if entry["status"] == "BootIssued":
        require(valid_instant(entry.get("boot_issued_at")) and "boot_verified_at" not in entry,
                "managed selection lacks its issued boot witness")
    elif entry["boot_generation"]:
        require(valid_instant(entry.get("boot_issued_at")) and valid_instant(entry.get("boot_verified_at")),
                "managed selection lacks verified boot timestamps")


def read_selection(directory):
    entry = optional_json(directory, SELECTION)
    if entry is not None:
        validate_selection(entry)
    return entry


def exists_at(directory, name):
    try:
        os.stat(name, dir_fd=directory, follow_symlinks=False)
        return True
    except FileNotFoundError:
        return False


def pending_publication(directory, generation):
    following = generation + 1
    if following < 2**63 and exists_at(directory, generation_name(following)):
        return following
    return None


def stable_selection(entry, expected_generation, expected_digest):
    seen = entry["generation"] if entry else 0
    require(seen == expected_generation, "expected selection generation changed")
    if entry is not None:
        require(entry["status"] == "Selected", "previous selected boot needs recovery")
        require(entry["selection_digest"] == expected_digest, "selected state advanced since adoption planning")


def signing_identity(signing):
    if signing is None:
        return None
    return signing["scheme"], signing["certificate_sha256"]


def check_successor(directory, existing, deployment, custody):
    if existing is None:
        return
    old = existing["deployment"]
    current = existing["current"]
    require(deployment["node_id"] == old["node_id"]
            and deployment["source_sha256"] == current["image_sha256"]
            and deployment["source_previous_receipt_sha256"] == old["candidate_receipt_sha256"]
            and deployment["firmware_code_sha256"] == current["efi_code_sha256"]
            and deployment["firmware_vars_sha256"] == current["efi_vars_sha256"]
            and signing_identity(deployment["secure_boot"]) == signing_identity(old["secure_boot"]),
            "candidate does not descend from the current selected disk, firmware and build receipt")
    previous = custody.enter_context(generation_directory(directory, generation_name(existing["generation"])))
    with open_file(previous, DISK, writable=False) as (disk, _):
        supplied = os.stat(deployment["source_disk"], follow_symlinks=False)
        require(os.path.samestat(supplied, os.fstat(disk)), "candidate source is not the current selected disk")
    check_material(previous, old, current)
    check_build_receipt(previous, old)
    return previous


def hold_source(custody, sources, path, expected, size, previous=None):
    observed = identity(path.stat(follow_symlinks=False))
    require(all(observed[:2] != tuple(saved[:2]) for _, saved in sources),
            "activation and predecessor custody require distinct files")
    saved = custody.enter_context(stopped_source(path, expected, size, previous if previous is not None else observed))
    sources.append((path, saved))


def hold_variables(custody, sources, directory, expected):
    with open_file(directory, VARS, writable=False) as (fd, size):
        hold_source(custody, sources, fd_path(directory) / VARS, expected, size, identity(os.fstat(fd)))


@contextmanager
def activation_context(directory, existing, path, expected):
    with lock_directory(path, owner=os.geteuid()) as activation, ExitStack() as custody:
        deployment, current, proof = prepare_activation(activation, path, expected)
        original = Path(deployment["source_disk"])
        managed = fd_path(activation) / DISK
        size = deployment["size_bytes"]
        sources = []
        hold_source(custody, sources, original, deployment["source_sha256"], size, deployment["source_identity"])
        hold_source(custody, sources, managed, current["image_sha256"], size)
        hold_variables(custody, sources, activation, current["efi_vars_sha256"])
        previous = check_successor(directory, existing, deployment, custody)
        if previous is not None:
            hold_variables(custody, sources, previous, existing["current"]["efi_vars_sha256"])
        yield activation, deployment, current, proof, sources


def verify_activation_sources(sources):
    for path, identity in sources:
        verify_stopped_source(path, identity)


def plan_adoption(directory, args):
    existing = read_selection(directory)
    expected_digest = existing["selection_digest"] if existing else None
    stable_selection(existing, args.expected_generation, expected_digest)
    next_generation = args.expected_generation + 1
    generation_name(next_generation)
    path = args.activation_dir.absolute()
    expected = checked_digest(args.activation_receipt_sha256)
    with activation_context(directory, existing, path, expected) as (_, deployment, current, proof, sources):
        plan = {
            "expected_generation": args.expected_generation,
            "expected_selection_digest": expected_digest,
            "generation": next_generation,
            "deployment": deployment,
            "current": current,
            "proof": proof,
        }
        approved = plan_digest(plan)
        selection_scratch.require_plan(directory, approved)
        previous = read_plan(directory)
        if previous is not None and previous["plan_digest"] == approved:
            return previous
        require(pending_publication(directory, args.expected_generation) is None,
                "published generation needs its original adoption plan")
        verify_activation_sources(sources)
        entry = {"format": 1, "status": "Planned", "planned_at": timestamp(),
                 "plan": plan, "plan_digest": approved}
        save_json(directory, PLAN, entry)
        return entry


def selected_entry(plan, approved, when):
    result = {
        "format": 1,
        "status": "Selected",
        "rollback_mode": ROLLBACK,
        "generation": plan["generation"],
        "boot_generation": 0,
        "selected_at": when,
        "adoption_plan_digest": approved,
        "previous_selection_digest": plan["expected_selection_digest"],
        "deployment": plan["deployment"],
        "current": plan["current"],
        "proof": plan["proof"],
    }
    result["selection_digest"] = selection_digest(result)
    return result


def check_adoption(directory, entry):
    original, _ = read_json(directory, ADOPTION)
    validate_selection(original)
    fields = ("generation", "adoption_plan_digest", "previous_selection_digest", "deployment", "selected_at")
    require(original["status"] == "Selected" and original["boot_generation"] == 0
            and all(entry[field] == original[field] for field in fields),
            "selected record differs from its original adoption")
    check_build_receipt(directory, entry["deployment"])
    previous = optional_json(directory, PREVIOUS)
    if entry["generation"] == 1:
        require(previous is None, "initial adoption unexpectedly contains a previous selection")
    else:
        require(previous is not None, "adopted generation lost its previous selection receipt")
        validate_selection(previous)
        require(previous["status"] == "Selected" and previous["generation"] == entry["generation"] - 1
                and previous["selection_digest"] == entry["previous_selection_digest"],
                "adopted generation has the wrong previous selection receipt")
    return original, previous


def recover_publication(directory, name, plan, approved, existing):
    with generation_directory(directory, name) as published:
        entry, _ = read_json(published, ADOPTION)
        _, previous = check_adoption(published, entry)
        require(entry == selected_entry(plan, approved, entry["selected_at"]),
                "published generation does not match the approved adoption")
        require(previous == existing, "published generation has a different predecessor receipt")
        check_material(published, entry["deployment"], entry["current"])
        os.fsync(published)
    os.fsync(directory)
    return entry


def adopt(directory, args):
    approved = checked_digest(args.plan_digest)
    existing = read_selection(directory)
    if existing is not None and existing["adoption_plan_digest"] == approved:
        report = inspect(directory)
        if report["status"] == "Selected":
            os.fsync(directory)
        return report
    pending = read_plan(directory)
    require(pending is not None and pending["plan_digest"] == approved,
            "approval differs from the exact pending adoption plan")
    plan = pending["plan"]
    stable_selection(existing, plan["expected_generation"], plan["expected_selection_digest"])
    deployment = plan["deployment"]
    name = generation_name(plan["generation"])
    path = Path(deployment["activation_dir"])
    expected = deployment["activation_receipt_sha256"]
    with activation_context(directory, existing, path, expected) as (activation, actual, current, proof, sources):
        require((actual, current, proof) == (deployment, plan["current"], plan["proof"]),
                "approved activation evidence changed before adoption")
        if exists_at(directory, name):
            entry = recover_publication(directory, name, plan, approved, existing)
        else:
            with selection_scratch.staging(directory, plan, approved) as (pair, target):
                entry = selected_entry(plan, approved, timestamp())
                copy_material(activation, target, current)
                copy_build_receipt(target, deployment, exclusive=True)
                check_material(target, deployment, current)
                if existing is not None:
                    write_json_exclusive(target, PREVIOUS, existing)
                write_json_exclusive(target, ADOPTION, entry)
                verify_activation_sources(sources)
                selection_scratch.verify_staging(directory, target, plan, approved)
                publish_bundle(pair, directory, name)
        verify_activation_sources(sources)
        selection_scratch.finish_publication(directory, plan, approved, name)
        save_json(directory, SELECTION, entry)
        return entry


def discard_staging(directory, args):
    approved = checked_digest(args.plan_digest)
    pending = read_plan(directory)
    require(pending is not None and pending["plan_digest"] == approved,
            "scratch discard approval differs from the exact pending adoption plan")
    plan = pending["plan"]
    return selection_scratch.discard(directory, plan, approved, generation_name(plan["generation"]))


def observe_unsettled_boot(directory, entry):
    report = {**entry, "status": "RecoveryRequired", "reason": "issued selected boot has no verified receipt"}
    try:
        with generation_directory(directory, generation_name(entry["generation"])) as selected:
            record, receipt_sha = selection_recovery.read(selected, validate_selection)
            if record["issued"] == entry:
                report.update(reason="a selected boot proof awaits exact reconciliation",
                              boot_proof_sha256=receipt_sha)
    except (OSError, ValueError, KeyError, TypeError, RecursionError):
        # A missing, malformed, or older proof cannot settle an issued boot.
        pass
    return report


def observed_selection(directory, entry):
    if entry["status"] == "BootIssued":
        return observe_unsettled_boot(directory, entry)
    try:
        with generation_directory(directory, generation_name(entry["generation"])) as selected:
            check_adoption(selected, entry)
            check_material(selected, entry["deployment"], entry["current"])
    except (OSError, ValueError) as error:
        return {**entry, "status": "RecoveryRequired", "reason": str(error)}
    return entry


def inspect(directory):
    current = read_selection(directory)
    if current is None:
        current = {"format": 1, "status": "Unselected", "generation": 0, "rollback_mode": ROLLBACK}
    else:
        current = observed_selection(directory, current)
    pending = pending_publication(directory, current["generation"])
    if current["status"] != "RecoveryRequired" and pending is not None:
        return {**current, "status": "RecoveryRequired", "reason": "published generation awaits exact adoption",
                "pending_generation": pending}
    scratch = selection_scratch.copy_status(directory)
    if current["status"] != "RecoveryRequired" and scratch is not None:
        return {**current, "status": "RecoveryRequired", "reason": "adoption staging awaits exact cleanup",
                "staging": scratch}
    return current


def boot_selected(directory, args):
    entry = read_selection(directory)
    require(entry is not None, "managed node has no selected deployment")
    require(entry["status"] == "Selected", "a selected boot was already issued; inspect the existing operation")
    require(checked_digest(args.selection_digest) == entry["selection_digest"],
            "approval differs from the exact current selection digest")
    require(pending_publication(directory, entry["generation"]) is None,
            "a published generation awaits exact adoption; inspect before booting")
    require(selection_scratch.copy_status(directory) is None,
            "an adoption staging copy needs exact cleanup before booting")
    next_boot = entry["boot_generation"] + 1
    require(next_boot < 2**63, "selected boot generation exhausted")

    issued = None

    def issue():
        nonlocal issued
        require(issued is None, "selected boot was already issued")
        issued = {**entry, "status": "BootIssued", "boot_issued_at": timestamp(),
                  "boot_attempt_id": str(uuid.uuid4())}
        issued.pop("boot_verified_at", None)
        selection_recovery.check_capacity(issued)
        save_json(directory, SELECTION, issued)

    with generation_directory(directory, generation_name(entry["generation"])) as selected:
        check_adoption(selected, entry)
        result = boot(fd_path(selected), entry, args, issue)
        require(issued is not None and result["current"]["boot_id"] != entry["current"]["boot_id"],
                "selected boot did not return fresh issued proof")
        check_proof(entry["deployment"], result["current"], result["proof"])
        updated = {**issued, **result, "status": "Selected", "boot_generation": next_boot,
                   "boot_verified_at": timestamp()}
        updated["selection_digest"] = selection_digest(updated)
        validate_selection(updated)
        with selection_recovery.final_artifacts(selected, updated) as recheck:
            selection_recovery.seal(selected, issued, updated)
            recheck()
            save_json(directory, SELECTION, updated)
        return updated


def reconcile_boot(directory, args):
    approved_selection = checked_digest(args.selection_digest)
    approved_proof = checked_digest(args.boot_proof_sha256)
    current = read_selection(directory)
    require(current is not None, "managed node has no selected deployment")
    generation = generation_name(current["generation"])
    require(pending_publication(directory, current["generation"]) is None
            and selection_scratch.copy_status(directory) is None,
            "managed adoption needs recovery before settling a selected boot")
    with generation_directory(directory, generation) as selected:
        check_adoption(selected, current)
        record, proof_sha = selection_recovery.read(selected, validate_selection)
        require(proof_sha == approved_proof and record["issued"]["selection_digest"] == approved_selection,
                "approval differs from the exact issued selection and boot proof")
        settled = record["settled"]
        require(current == record["issued"] or current == settled,
                "boot proof does not settle the current issued attempt")
        with selection_recovery.final_artifacts(selected, settled) as recheck:
            os.fsync(selected)
            recheck()
            if current != settled:
                save_json(directory, SELECTION, settled)
            else:
                os.fsync(directory)
        return settled


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan")
    plan.add_argument("--activation-dir", type=Path, required=True)
    plan.add_argument("--activation-receipt-sha256", required=True)
    plan.add_argument("--expected-generation", type=int, required=True)
    apply = commands.add_parser("adopt")
    apply.add_argument("--plan-digest", required=True)
    discard = commands.add_parser("discard-staging")
    discard.add_argument("--plan-digest", required=True)
    read = commands.add_parser("inspect")
    run = commands.add_parser("boot-selected")
    run.add_argument("--selection-digest", required=True)
    run.add_argument("--root-console-password-file", type=Path, required=True)
    reconcile = commands.add_parser("reconcile-boot")
    reconcile.add_argument("--selection-digest", required=True)
    reconcile.add_argument("--boot-proof-sha256", required=True)
    for command in (plan, apply, discard, read, run, reconcile):
        command.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(os.geteuid() != 0 and sys.platform == "linux" and platform.machine() == "x86_64",
                "managed native selection requires an unprivileged local Linux amd64 operator")
        with lock_directory(args.state_dir, owner=os.geteuid()) as directory:
            if args.command == "plan":
                result = plan_adoption(directory, args)
            elif args.command == "adopt":
                result = adopt(directory, args)
            elif args.command == "discard-staging":
                result = discard_staging(directory, args)
            elif args.command == "boot-selected":
                result = boot_selected(directory, args)
            elif args.command == "reconcile-boot":
                result = reconcile_boot(directory, args)
            else:
                result = inspect(directory)
        print(json.dumps(result))
        return 1 if result["status"] == "RecoveryRequired" else 0
    except (OSError, ValueError, KeyError, TypeError, RecursionError, subprocess.SubprocessError) as error:
        parser.exit(1, f"selection stopped: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
