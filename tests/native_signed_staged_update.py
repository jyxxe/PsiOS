#!/usr/bin/env python3
"""Require real Secure Boot evidence before publishing a staged native update."""
import argparse
import hashlib
import json
from pathlib import Path
import secrets
import shlex
import shutil
import subprocess
import sys
import tempfile
import uuid

import pefile

from native_disk import REPO, password, sha
from native_secure_boot import EFI_FILE, check_firmware, create_signer, secure_vm, signing_args
from native_staged_update import digest_map, durable_fact, invoke, refused, succeeded, validate_bundle
from native_upgrade import shutdown


def cert_sha256(cert):
    der = subprocess.check_output(["openssl", "x509", "-in", cert, "-outform", "DER"],
                                  stdin=subprocess.DEVNULL, timeout=10)
    return hashlib.sha256(der).hexdigest()


def verify_uki(work, disk, build, cert, other_cert):
    uki = work / "candidate.efi"
    succeeded(invoke(["mcopy", "-i", f"{disk}@@1048576", EFI_FILE, uki], 30))
    assert sha(uki) == build["efi_application_sha256"]
    succeeded(invoke(["sbverify", "--cert", cert, uki], 30))
    assert invoke(["sbverify", "--cert", other_cert, uki], 30).returncode != 0
    image = pefile.PE(str(uki), fast_load=True)
    try:
        sections = {section.Name.rstrip(b"\x00") for section in image.sections if section.SizeOfRawData > 0}
        assert {b".linux", b".initrd", b".cmdline"} <= sections, sections
    finally:
        image.close()


def boot_operator(console, operator, admin, build):
    console.login("root", admin)
    check_firmware(console)
    console.send("exit")
    console.login("psios", operator)
    manifest = json.loads(console.run("cat /psios-deployment.json"))
    assert manifest["secure_boot"] == build["secure_boot"], manifest
    assert console.run("cat /psios-source") == build["source_commit"]
    assert console.run("sha256sum /psios-deployment.json").split()[0] == build["deployment_manifest_sha256"]
    if build.get("secure_boot_enforcement_required"):
        status = json.loads(console.run("psiosctl native-status"))
        assert status["secure_boot_required"] is True and status["secure_boot_observation"] == "Enforced", status
        assert status["boot_id"] == console.run("cat /proc/sys/kernel/random/boot_id")
        assert status["deployment_manifest_sha256"] == "sha256:" + build["deployment_manifest_sha256"]
        assert console.run("stat -c '%u:%g:%a:%h' /run/psios-boot.json") == "0:0:444:1"
        console.run("test ! -w /run/psios-boot.json")
    return durable_fact(console)


def isolated_boot(work, disk, code, original_vars, credentials, build, baseline, token, *, pace_commands=False):
    operator, admin = credentials
    with tempfile.TemporaryDirectory(dir=work, prefix="secure-boot-view-") as directory:
        view = Path(directory)
        shutil.copyfile(code, view / "code.fd")
        shutil.copyfile(original_vars, view / "enrolled.fd")
        assert sha(view / "enrolled.fd") == sha(original_vars)
        overlay = view / "boot.qcow2"
        succeeded(invoke(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw",
                          "-b", str(disk), overlay], 30))
        with secure_vm(view, overlay, disk_format="qcow2", pace_commands=pace_commands) as console:
            observed = boot_operator(console, operator, admin, build)
            assert observed[0] == baseline[0], observed
            assert observed[1]["task_id"] == baseline[1]["task_id"] and observed[1]["deduplicated"] is True
            assert observed[2] == baseline[2] and observed[4] == baseline[4], observed
            assert observed[3] != baseline[3], observed
            assert console.run("cat $HOME/signed-staged-update-fact") == token
            shutdown(console, admin)
    return observed[3]


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    key, cert = create_signer(work, "staged-enrolled")
    other_key, other_cert = create_signer(work, "staged-untrusted")
    signer = signing_args(args, key, cert)
    signer_digest = cert_sha256(cert)
    assert cert_sha256(other_cert) != signer_digest
    code = work / "code.fd"
    plain_vars = work / "plain-vars.fd"
    enrolled = work / "enrolled.fd"
    original_vars = work / "original-vars.fd"
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.secboot.fd", code)
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", plain_vars)
    owner = str(uuid.uuid4())
    succeeded(invoke(["virt-fw-vars", "-i", plain_vars, "-o", enrolled, "--set-pk", owner, cert,
                      "--add-kek", owner, cert, "--add-db", owner, cert, "--sb"], 30))
    assert sha(enrolled) != sha(plain_vars)

    old = work / "signed-before.raw"
    previous = work / "signed-before.json"
    built = succeeded(invoke([sys.executable, "-B", args.previous_installer, old,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.previous_archive,
        "--core-sha256", sha(args.previous_archive), "--console-hash", operator_hash,
        "--root-console-hash", admin_hash, *signer], 330))
    previous.write_text(built)
    previous.chmod(0o600)
    original_build = json.loads(built)
    signed = original_build["secure_boot"]
    assert signed["certificate_sha256"] == signer_digest and signed["efi_stub_sha256"] == sha(args.efi_stub), signed
    assert sha(old) == original_build["initial_image_sha256"], original_build
    token = secrets.token_hex(12)
    with secure_vm(work, old, vars_file=original_vars.name) as console:
        baseline = boot_operator(console, operator, admin, original_build)
        console.run(f"printf '%s\\n' {shlex.quote(token)} > $HOME/signed-staged-update-fact; sync")
        shutdown(console, admin)

    frozen = digest_map((old, previous, code, original_vars, enrolled, plain_vars, key, cert))
    archive_digest = sha(args.archive)
    assert frozen[old] != original_build["initial_image_sha256"], "old signed guest did not persist the fixture"
    assert frozen[original_vars] != frozen[plain_vars], "fixture lost its enrolled variable store"

    def stage(out, signing=signer, variables=original_vars):
        return [sys.executable, "-B", args.controller, out, "--upgrade-from", old,
                "--previous-receipt", previous, "--previous-receipt-sha256", frozen[previous],
                "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", archive_digest,
                "--ovmf-code", code, "--ovmf-code-sha256", frozen[code],
                "--ovmf-vars", variables, "--ovmf-vars-sha256", frozen[variables], *signing]

    wrong = work / "must-not-publish-other-signer"
    refused(work, stage(wrong, signing_args(args, other_key, other_cert)), r"original enrolled certificate|existing signing certificate")
    assert not wrong.exists(), wrong
    omitted = work / "must-not-publish-unsigned"
    refused(work, stage(omitted, ()), r"existing signing certificate|signing.*requir|requir.*signing")
    assert not omitted.exists(), omitted
    insecure = work / "must-not-publish-setup-mode"
    refused(work, stage(insecure, variables=plain_vars),
            r"failed at secure-boot|secure.?boot.not.enforced|not attest.*secure.?boot|setup mode", timeout=850)
    assert not insecure.exists(), insecure
    assert digest_map(frozen) == frozen

    output = work / "signed-staged"
    before = {entry.name for entry in work.iterdir()}
    stdout = succeeded(invoke(stage(output), 850))
    assert {entry.name for entry in work.iterdir()} == before | {output.name}, "stage leaked private scratch paths"
    build = validate_bundle(args, output, stdout, original_build, frozen, old, previous, code, original_vars, baseline[0])
    receipt = json.loads((output / "staged-update.json").read_bytes())
    assert build["secure_boot"] == signed and receipt["candidate"]["secure_boot"] == signed, receipt
    assert receipt["trial"]["secure_boot"] == {**signed, "status": "Enforced"}, receipt["trial"]
    assert receipt["trial"]["secure_boot"]["certificate_sha256"] == signer_digest
    assert digest_map(frozen) == frozen
    candidate = output / "candidate.raw"
    bundle = digest_map(output.iterdir())
    verify_uki(work, candidate, build, cert, other_cert)
    credentials = (operator, admin)
    active_boot = isolated_boot(work, candidate, code, original_vars, credentials, build, baseline, token)
    old_boot = isolated_boot(work, old, code, original_vars, credentials, original_build, baseline, token)
    assert active_boot != old_boot
    assert digest_map(frozen) == frozen and digest_map(bundle) == bundle
    print("PASS signed staged update, exact UKI validation, and independent enforced Secure Boot")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--previous-installer", type=Path, default=REPO / "images/native/create.py")
    for flag in ("--efi-stub", "--kernel-iso", "--previous-archive", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--expected-source-format", type=int)
    parser.add_argument("--expected-target-format", type=int)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-signed-staged-update-") as directory:
        exercise(args, Path(directory).resolve())


if __name__ == "__main__":
    main()
