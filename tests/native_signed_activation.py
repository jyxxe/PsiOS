#!/usr/bin/env python3
"""Verify an approved signed native first boot under actually enforced Secure Boot."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import sys
import tempfile
import uuid

from native_activation import Activation, JOURNAL, assert_private, managed_bytes, no_qemu, rejected
from native_selection_recovery import exercise_selection_recovery
from native_disk import REPO, password, sha
from native_secure_boot import create_signer, secure_vm, signing_args
from native_signed_staged_update import boot_operator, cert_sha256, isolated_boot, verify_uki
from native_staged_update import digest_map, invoke, succeeded, validate_bundle
from native_upgrade import shutdown


def reject_disabled_firmware(work, disk, code, blank_vars, expected_disk):
    # Run the same signed UKI with setup-mode firmware. The guest itself must
    # stop before normal authority startup, without writing its backing volume.
    with tempfile.TemporaryDirectory(dir=work, prefix="secure-boot-disabled-") as directory:
        view = Path(directory)
        shutil.copyfile(code, view / "code.fd")
        shutil.copyfile(blank_vars, view / "enrolled.fd")
        overlay = view / "guard.qcow2"
        succeeded(invoke(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw",
                          "-b", disk, overlay], 30))
        map_command = ["qemu-img", "map", "--output=json", "-f", "qcow2", overlay]
        before = json.loads(succeeded(invoke(map_command, 30)))
        with secure_vm(view, overlay, disk_format="qcow2") as console:
            prefix = console.until(b"PsiOS boot stopped at secure-boot-not-enforced; use external rescue media",
                                   timeout=100, reject=b"psios login:")
            assert b"psiosd ready" not in prefix
            assert console.vm.wait(35) == 0
        assert json.loads(succeeded(invoke(map_command, 30))) == before, "denied guest allocated disk writes"
        succeeded(invoke(["qemu-img", "compare", "-q", "-f", "qcow2", "-F", "raw",
                          overlay, disk], 95))
    assert sha(disk) == expected_disk


def reject_wrong_variables(args, work, fixture, blank_vars):
    bad_plan = work / "untrusted-firmware"
    bad_plan.mkdir(mode=0o700)
    untrusted = Activation(args.activator, fixture.stage, fixture.source, fixture.code, blank_vars, fixture.secret)
    result = no_qemu(untrusted.plan_command(bad_plan), work / "untrusted-firmware.strace")
    rejected(result, r"SHA-256 mismatch|(?:OVMF|firmware|variable).*digest")
    assert not list(bad_plan.iterdir()), "denied plan published managed files"

    changed = work / "modified-firmware"
    plan = fixture.planned(changed)
    original = (changed / JOURNAL).read_bytes()
    variables = changed / "managed-efi-vars.fd"
    assert sha(variables) == sha(fixture.variables) != sha(blank_vars)
    shutil.copyfile(blank_vars, variables)
    before = managed_bytes(changed)
    result = no_qemu(fixture.apply(changed, plan["plan_digest"]), work / "managed-firmware-drift.strace")
    rejected(result, r"managed boot inputs changed|(?:OVMF|firmware|variable).*digest")
    assert (changed / JOURNAL).read_bytes() == original
    assert managed_bytes(changed) == before and sha(variables) == sha(blank_vars)
    shutil.rmtree(changed)


def activate(fixture, work, candidate, signed, baseline, admin):
    state = work / "signed-verified"
    plan = fixture.planned(state)
    assert plan["plan"]["secure_boot"] == signed, plan
    assert plan["plan"]["firmware_vars_sha256"] == sha(fixture.variables), plan
    assert plan["plan"]["candidate_sha256"] == candidate["initial_image_sha256"], plan
    command = fixture.apply(state, plan["plan_digest"])
    result = invoke(command, 300)
    report = json.loads(succeeded(result))
    assert report["status"] == "BootVerified" and report["rollback_mode"] == "ManualRecoveryOnly", report
    assert report["plan"] == plan["plan"] and report["plan_digest"] == plan["plan_digest"], report
    assert report["planned_at"] <= report["boot_issued_at"] <= report["boot_verified_at"], report
    proof = report["proof"]
    boot_id = proof["boot_id"]
    assert str(uuid.UUID(boot_id)) == boot_id and boot_id != baseline[3]
    assert proof == {
        "boot_id": boot_id,
        "node_id": baseline[0],
        "deployment_manifest_sha256": candidate["deployment_manifest_sha256"],
        "secure_boot": {**signed, "status": "Enforced"},
    }, proof
    saved = (state / JOURNAL).read_bytes()
    assert json.loads(saved) == report and admin.encode() not in saved
    assert admin not in result.stdout + result.stderr
    assert_private(state / JOURNAL)
    observed = managed_bytes(state)
    for path in observed:
        assert_private(path)
    assert report["managed"] == {
        "disk": "managed.raw",
        "efi_code": "managed-ovmf-code.fd",
        "efi_vars": "managed-efi-vars.fd",
        "image_sha256": observed[state / "managed.raw"],
        "efi_code_sha256": observed[state / "managed-ovmf-code.fd"],
        "efi_vars_sha256": observed[state / "managed-efi-vars.fd"],
    }, report
    assert observed[state / "managed-ovmf-code.fd"] == sha(fixture.code)
    for name, repeat in (("inspect", fixture.inspect(state)), ("replay", command)):
        returned = no_qemu(repeat, work / f"signed-{name}.strace")
        assert json.loads(succeeded(returned)) == report
    assert (state / JOURNAL).read_bytes() == saved and managed_bytes(state) == observed
    return state, observed, boot_id


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    secret = work / "root-console"
    with os.fdopen(os.open(secret, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
        stream.write(admin + "\n")
    assert_private(secret)

    key, cert = create_signer(work, "activation-enrolled")
    _, other_cert = create_signer(work, "activation-untrusted")
    signer = signing_args(args, key, cert)
    code = work / "code.fd"
    blank_vars = work / "blank-vars.fd"
    enrolled = work / "enrolled.fd"
    old_vars = work / "old-vars.fd"
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.secboot.fd", code)
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", blank_vars)
    owner = str(uuid.uuid4())
    succeeded(invoke(["virt-fw-vars", "-i", blank_vars, "-o", enrolled, "--set-pk", owner, cert,
                      "--add-kek", owner, cert, "--add-db", owner, cert, "--sb"], 30))
    assert sha(enrolled) != sha(blank_vars)

    old = work / "signed-before.raw"
    previous = work / "signed-before.json"
    built = succeeded(invoke([sys.executable, "-B", args.previous_installer, old,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.previous_archive,
        "--core-sha256", sha(args.previous_archive), "--console-hash", operator_hash,
        "--root-console-hash", admin_hash, *signer], 330))
    previous.write_text(built)
    original = json.loads(built)
    signed = original["secure_boot"]
    assert signed["certificate_sha256"] == cert_sha256(cert) and signed["efi_stub_sha256"] == sha(args.efi_stub)
    assert sha(old) == original["initial_image_sha256"]
    token = secrets.token_hex(12)
    with secure_vm(work, old, vars_file=old_vars.name) as console:
        baseline = boot_operator(console, operator, admin, original)
        console.run(f"printf '%s\\n' {shlex.quote(token)} > $HOME/signed-staged-update-fact; sync")
        shutdown(console, admin)

    frozen = digest_map((old, previous, code, old_vars, enrolled, blank_vars, key, cert))
    assert frozen[old] != original["initial_image_sha256"] and frozen[old_vars] != frozen[blank_vars]
    stage = work / "signed-staged"
    stdout = succeeded(invoke([sys.executable, "-B", args.controller, stage, "--upgrade-from", old,
        "--previous-receipt", previous, "--previous-receipt-sha256", frozen[previous],
        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive),
        "--ovmf-code", code, "--ovmf-code-sha256", frozen[code],
        "--ovmf-vars", old_vars, "--ovmf-vars-sha256", frozen[old_vars], *signer], 850))
    candidate = validate_bundle(args, stage, stdout, original, frozen, old, previous, code, old_vars, baseline[0])
    staged = json.loads(stdout)
    assert candidate["secure_boot"] == staged["candidate"]["secure_boot"] == signed, staged
    assert candidate["secure_boot_enforcement_required"] is True, candidate
    assert staged["candidate"]["secure_boot_enforcement_required"] is True, staged
    assert staged["trial"]["secure_boot"] == {**signed, "status": "Enforced"}, staged
    bundle = digest_map(stage.iterdir())
    disk = stage / "candidate.raw"
    verify_uki(work, disk, candidate, cert, other_cert)
    reject_disabled_firmware(work, disk, code, blank_vars, bundle[disk])

    fixture = Activation(args.activator, stage, old, code, old_vars, secret)
    reject_wrong_variables(args, work, fixture, blank_vars)
    assert digest_map(bundle) == bundle and digest_map(frozen) == frozen
    state, managed, activated_boot = activate(fixture, work, candidate, signed, baseline, admin)
    selected_dir, selected = exercise_selection_recovery(work, args.selector, state, secret)
    assert selected["proof"]["secure_boot"] == {**signed, "status": "Enforced"}, selected
    secret.unlink()
    credentials = (operator, admin)
    new_boot = isolated_boot(work, selected_dir / "managed.raw", selected_dir / "managed-ovmf-code.fd",
                             selected_dir / "managed-efi-vars.fd", credentials, candidate, baseline, token,
                             pace_commands=True)
    old_boot = isolated_boot(work, old, code, old_vars, credentials, original, baseline, token)
    assert len({baseline[3], activated_boot, selected["proof"]["boot_id"], new_boot, old_boot}) == 5
    assert digest_map(frozen) == frozen and digest_map(bundle) == bundle
    assert managed_bytes(state) == managed
    print("PASS signed native activation and boot reconciliation, Secure Boot enforcement and durable Task and home data")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--activator", type=Path, default=REPO / "images/native/activate_update.py")
    parser.add_argument("--selector", type=Path, default=REPO / "images/native/select_update.py")
    parser.add_argument("--previous-installer", type=Path, default=REPO / "images/native/create.py")
    for name in ("efi-stub", "kernel-iso", "previous-archive", "archive"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--work-parent", type=Path)
    parser.add_argument("--expected-source-format", type=int)
    parser.add_argument("--expected-target-format", type=int)
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error("run the signed activation acceptance as an unprivileged Linux operator")
    with tempfile.TemporaryDirectory(dir=args.work_parent, prefix="psios-native-signed-activation-") as directory:
        exercise(args, Path(directory).resolve())


if __name__ == "__main__":
    main()
