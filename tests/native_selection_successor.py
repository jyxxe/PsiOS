"""Advance an existing selected deployment through a real second isolated update."""

import argparse
import json
import sys

from native_activation import Activation, JOURNAL, MATERIALIZED, no_qemu, rejected
from native_disk import sha
from native_selection import Selection, checked_material, snapshot, stop_after_publication
from native_staged_update import digest_map, invoke, succeeded, validate_bundle


def exercise_successor(args, work, previous_dir, selected, secret):
    previous_bytes = snapshot(previous_dir.iterdir())
    source = previous_dir / "managed.raw"
    receipt = previous_dir / "candidate-build.json"
    code = previous_dir / "managed-ovmf-code.fd"
    variables = previous_dir / "managed-efi-vars.fd"
    existing = json.loads(receipt.read_bytes())
    frozen = digest_map((source, receipt, code, variables))
    archive = args.successor_archive or args.archive
    stage = work / "successor-stage"
    result = invoke([sys.executable, "-B", args.controller, stage,
        "--upgrade-from", source, "--previous-receipt", receipt, "--previous-receipt-sha256", frozen[receipt],
        "--kernel-iso", args.kernel_iso, "--core-archive", archive, "--core-sha256", sha(archive),
        "--ovmf-code", code, "--ovmf-code-sha256", frozen[code],
        "--ovmf-vars", variables, "--ovmf-vars-sha256", frozen[variables]], 850)
    expectation = argparse.Namespace(archive=archive, expected_source_format=selected["deployment"]["database_format"],
                                     expected_target_format=args.expected_successor_format)
    candidate = validate_bundle(expectation, stage, succeeded(result), existing, frozen, source, receipt,
                                code, variables, selected["proof"]["node_id"])
    if args.successor_archive is not None and sha(archive) != sha(args.archive):
        assert candidate["deployment_manifest_sha256"] != selected["proof"]["deployment_manifest_sha256"]
    staged = snapshot(stage.iterdir())

    activation = Activation(args.activator, stage, source, code, variables, secret)
    verified_dir = work / "successor-first-boot"
    first_plan = activation.planned(verified_dir)
    verified = json.loads(succeeded(invoke(activation.apply(verified_dir, first_plan["plan_digest"]), 300)))
    assert verified["status"] == "BootVerified" and verified["proof"]["node_id"] == selected["proof"]["node_id"]
    verified_bytes = snapshot(verified_dir / name for name in MATERIALIZED | {JOURNAL})

    selector = Selection(args.selector, verified_dir, secret)
    state = previous_dir.parent
    plan = json.loads(succeeded(invoke(selector.plan(state, selected["generation"]), 180)))
    assert plan["plan"]["expected_selection_digest"] == selected["selection_digest"]
    assert plan["plan"]["deployment"]["source_previous_receipt_sha256"] == frozen[receipt]
    adopted = stop_after_publication(work, selector, state, plan, verified)
    directory = state / "generation-2"
    retained = (directory / "previous-selection.json").read_bytes()
    assert json.loads(retained) == selected
    assert adopted["previous_selection_digest"] == selected["selection_digest"]
    rejected(no_qemu(selector.boot(state, selected["selection_digest"]), work / "retired-selection.strace"),
             r"digest|approval|exact")
    latest = json.loads(succeeded(invoke(selector.boot(state, adopted["selection_digest"]), 300)))
    assert latest["status"] == "Selected" and latest["generation"] == 2 and latest["boot_generation"] == 1
    assert latest["previous_selection_digest"] == selected["selection_digest"]
    assert latest["proof"]["node_id"] == selected["proof"]["node_id"]
    assert (directory / "previous-selection.json").read_bytes() == retained
    checked_material(directory, latest)
    assert snapshot(previous_bytes) == previous_bytes
    assert snapshot(verified_bytes) == verified_bytes and snapshot(staged) == staged
    print("PASS selected successor lineage, interrupted pointer publication and preserved previous generation")
    return directory, latest, candidate, verified["proof"]["boot_id"]
