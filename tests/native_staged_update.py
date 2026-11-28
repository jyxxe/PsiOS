#!/usr/bin/env python3
"""Stage a real native update and independently boot both preserved deployment views."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_state, extract_partition, password, replace_partition, sha
from native_upgrade import STATE_MIB, STATE_OFFSET, shutdown


MIB = 1024 * 1024
LARGE_COPY_MIB = 192
SOURCE_NIC = "52:54:00:58:3e:25"


def invoke(command, timeout):
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                          start_new_session=True) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=15)
            raise
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def succeeded(result):
    assert result.returncode == 0, (result.returncode, result.stderr[-2400:], result.stdout[-1000:])
    return result.stdout


def digest_map(files):
    return {path: sha(path) for path in files}


def refused(work, command, message, *, timeout=100):
    before = {path.name for path in work.iterdir()}
    result = invoke(command, timeout)
    assert result.returncode != 0, result.stdout[-1000:]
    assert re.search(message, result.stderr + result.stdout, re.I), result.stderr[-2400:]
    assert {path.name for path in work.iterdir()} == before, "refusal left filesystem changes in the work directory"
    return result


def durable_fact(console):
    authority, task, result, boot_id = check_state(console)
    assert result["availability"] == "Available", result
    output, = result["artifact_refs"]
    content = call(console, "Artifact.Read", {"reference": output})
    assert content["authority_id"] == authority and content["kernel_release"], content
    return authority, task, result, boot_id, content


def install_large_wal(work, disk, build):
    """Preserve a real committed, uncheckpointed SQLite WAL on the test disk."""
    state_size = build.get("geometry_mib", {}).get("state_mib", STATE_MIB)
    with tempfile.TemporaryDirectory(dir=work, prefix="budget-fixture-") as directory:
        scratch = Path(directory)
        state = scratch / "state.ext4"
        db = scratch / "authority.db"
        extract_partition(disk, state, STATE_OFFSET, state_size)
        succeeded(invoke(["debugfs", "-R", f"dump /psios/state.db {db}", state], 25))
        data = scratch / "copied.db"
        wal = scratch / "copied.db-wal"
        with closing(sqlite3.connect(db)) as connection:
            assert connection.execute("PRAGMA journal_mode=WAL").fetchone() == ("wal",)
            connection.execute("PRAGMA wal_autocheckpoint=0")
            with connection:
                connection.execute("CREATE TABLE upgrade_budget_fixture (payload BLOB NOT NULL)")
                connection.execute("INSERT INTO upgrade_budget_fixture VALUES(zeroblob(?))", (72 * MIB,))
            assert connection.execute("PRAGMA quick_check").fetchone() == ("ok",)
            shutil.copyfile(db, data)
            shutil.copyfile(db.with_suffix(".db-wal"), wal)
        assert data.stat().st_size < 64 * MIB < wal.stat().st_size
        commands = []
        for path, name in ((data, "state.db"), (wal, "state.db-wal")):
            target = f"/psios/{name}"
            commands.extend((f"rm {target}", f"write {path} {target}",
                             f"set_inode_field {target} mode 0100600",
                             f"set_inode_field {target} uid 1000", f"set_inode_field {target} gid 1000"))
        subprocess.run(["debugfs", "-w", "-f", "-", state], input="\n".join(commands) + "\n",
                       text=True, capture_output=True, check=True, timeout=45)
        replace_partition(disk, state, STATE_OFFSET)
        return data.stat().st_size + wal.stat().st_size


def boot_view(work, disk, source_vars, receiver, operator, admin, build, baseline, token):
    # Preserve both the sealed raw bytes and the original private EFI variables
    # while still booting the actual disk through the normal native path.
    image = work / f"{receiver}.qcow2"
    vars_file = work / f"{receiver}-vars.fd"
    shutil.copyfile(source_vars, vars_file)
    assert sha(source_vars) == sha(vars_file)
    succeeded(invoke(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw",
                      "-b", str(disk), str(image)], 20))
    with boot(work, image, disk_format="qcow2", vars_file=vars_file.name,
              network_mac=SOURCE_NIC, pace_commands=True) as console:
        console.login("psios", operator)
        observed = durable_fact(console)
        assert observed[0] == baseline[0], observed
        assert observed[1]["task_id"] == baseline[1]["task_id"], observed
        assert observed[1]["deduplicated"] is True, observed
        assert observed[2] == baseline[2] and observed[4] == baseline[4], observed
        assert observed[3] != baseline[3], observed
        assert console.run("cat $HOME/native-staged-update-fact") == token
        assert console.run("cat /psios-source") == build["source_commit"]
        deployment = console.run("sha256sum /psios-deployment.json").split()[0]
        assert deployment == build["deployment_manifest_sha256"], (receiver, deployment)
        shutdown(console, admin)
    image.unlink()
    vars_file.unlink()
    return observed[3]


def validate_bundle(args, output, stdout, old, frozen, source, receipt, code, vars_file, authority):
    products = {"candidate.raw", "candidate-build.json", "staged-update.json"}
    assert output.is_dir() and not output.is_symlink()
    assert {path.name for path in output.iterdir()} == products, list(output.iterdir())
    for name in products:
        path = output / name
        assert path.is_file() and not path.is_symlink(), path
    stage = json.loads((output / "staged-update.json").read_bytes())
    assert json.loads(stdout) == stage, stage
    assert (stage["format"], stage["status"], stage["rollback_mode"]) == (
        1, "Staged", "ManualRecoveryOnly"
    ), stage
    selected = stage["source"]
    trial = stage["trial"]
    staged = stage["candidate"]
    active_build = output / "candidate-build.json"
    build = json.loads(active_build.read_bytes())
    assert selected["image_sha256"] == frozen[source], selected
    assert selected["previous_receipt_sha256"] == frozen[receipt], selected
    assert selected["node_id"] == authority, selected
    assert trial["normalized_source_sha256"] == selected["normalized_image_sha256"], trial
    assert re.fullmatch(r"[0-9a-f]{64}", selected["normalized_image_sha256"]), selected
    assert re.fullmatch(r"[0-9a-f]{64}", trial["image_sha256"]), trial
    assert re.fullmatch(r"[0-9a-f]{64}", trial["deployment_manifest_sha256"]), trial
    assert stage["inputs"]["firmware_code_sha256"] == frozen[code], stage["inputs"]
    assert stage["inputs"]["firmware_vars_sha256"] == frozen[vars_file], stage["inputs"]
    assert stage["inputs"]["core_archive_sha256"] == sha(args.archive), stage["inputs"]
    assert staged["build_receipt_sha256"] == sha(active_build), staged
    assert staged["image_sha256"] == build["initial_image_sha256"] == sha(output / "candidate.raw"), staged
    assert Path(build["image"]) == output / "candidate.raw", build
    assert staged["deployment_manifest_sha256"] == build["deployment_manifest_sha256"], staged
    assert staged["source_commit"] == build["source_commit"], staged
    assert (build["state_uuid"], build["home_uuid"]) == (old["state_uuid"], old["home_uuid"]), build
    origin = build["upgrade"]
    assert origin["previous_image_sha256"] == frozen[source], origin
    assert origin["previous_source_commit"] == old["source_commit"], origin
    original = origin["verified_authority"]
    assert original["node_id"] == authority and original["database_format"] == selected["database_format"], original
    rehearsal = original["migration_rehearsal"]
    inspected = trial["verified_authority"]
    assert rehearsal["status"] == "Verified" and rehearsal["source_format"] == selected["database_format"], rehearsal
    assert inspected["database_format"] == rehearsal["target_format"], inspected
    assert inspected["node_id"] == authority and inspected["migration_required"] is False, inspected
    if args.expected_source_format is not None:
        assert selected["database_format"] == args.expected_source_format, selected
    if args.expected_target_format is not None:
        assert inspected["database_format"] == args.expected_target_format, inspected
    return build


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    old = work / "before.raw"
    receipt = work / "before.json"
    code = work / "code.fd"
    vars_file = work / "before-vars.fd"
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", code)
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", vars_file)
    vars_file.chmod(0o600)
    original = succeeded(invoke([sys.executable, "-B", args.previous_installer, old,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.previous_archive,
        "--core-sha256", sha(args.previous_archive), "--console-hash", operator_hash,
        "--root-console-hash", admin_hash], 330))
    receipt.write_text(original)
    receipt.chmod(0o600)
    old_build = json.loads(original)
    assert sha(old) == old_build["initial_image_sha256"], old_build
    pins = {"receipt": sha(receipt), "archive": sha(args.archive), "code": sha(code)}

    def stage(out, *, receipt_digest=None, code_digest=None, max_copy_mib=128):
        return [sys.executable, "-B", args.controller, out, "--upgrade-from", old,
                "--previous-receipt", receipt, "--previous-receipt-sha256", receipt_digest or pins["receipt"],
                "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", pins["archive"],
                "--ovmf-code", code, "--ovmf-code-sha256", code_digest or pins["code"],
                "--ovmf-vars", vars_file, "--ovmf-vars-sha256", sha(vars_file),
                "--max-copy-mib", str(max_copy_mib)]

    token = secrets.token_hex(12)
    live = work / "must-not-publish-running"
    initial_vars = sha(vars_file)
    # The NIC changes the source disk's PCI path, even with no network driver
    # configured in the guest. The isolated trial has the disk alone.
    with boot(work, old, vars_file=vars_file.name, network_mac=SOURCE_NIC, pace_commands=True) as console:
        console.login("psios", operator)
        baseline = durable_fact(console)
        console.run(f"printf '%s\\n' {shlex.quote(token)} > $HOME/native-staged-update-fact; sync")
        refused(work, stage(live), r"lock|in use|running|busy")
        assert not live.exists(), live
        after = durable_fact(console)
        assert after[0] == baseline[0] and after[2:] == baseline[2:], after
        assert console.run("cat $HOME/native-staged-update-fact") == token
        shutdown(console, admin)

    large_copy = install_large_wal(work, old, old_build) if args.large_authority else None
    frozen = digest_map((old, receipt, code, vars_file))
    assert frozen[old] != old_build["initial_image_sha256"], "old guest did not persist its new state"
    assert frozen[vars_file] != initial_vars, "the source firmware did not record its boot entry"
    rejected = work / "must-not-publish-untrusted"
    refused(work, stage(rejected, receipt_digest="0" * 64), r"receipt|digest|sha.?256")
    assert not rejected.exists(), rejected
    refused(work, stage(rejected, code_digest="0" * 64), r"firmware|ovmf|digest|sha.?256")
    assert not rejected.exists(), rejected
    occupied = work / "existing"
    occupied.mkdir(mode=0o700)
    sentinel = occupied / "prior-result"
    sentinel.write_bytes(b"existing installation data")
    refused(work, stage(occupied), r"exists|already|occupied")
    assert {path.name for path in occupied.iterdir()} == {sentinel.name}
    assert sentinel.read_bytes() == b"existing installation data"
    assert digest_map(frozen) == frozen

    if large_copy is not None:
        small = work / "must-not-publish-under-budget"
        direct = [sys.executable, "-B", args.controller.with_name("create.py"), small,
                  "--upgrade-from", old, "--previous-receipt", receipt, "--kernel-iso", args.kernel_iso,
                  "--core-archive", args.archive, "--core-sha256", pins["archive"]]
        refused(work, direct, r"authority-QuotaExceeded", timeout=260)
        refused(work, stage(small, max_copy_mib=64), r"authority-QuotaExceeded", timeout=300)
        assert not small.exists() and digest_map(frozen) == frozen
        direct[3] = work / "direct-upgrade.raw"
        report = json.loads(succeeded(invoke([*direct, "--max-copy-mib", str(LARGE_COPY_MIB)], 330)))
        observed = report["upgrade"]["verified_authority"]
        assert observed["max_copy_mib"] == LARGE_COPY_MIB and observed["copied_bytes"] == large_copy, observed
        assert observed["node_id"] == baseline[0] and observed["witness"] == "Confirmed", observed
        assert observed["migration_rehearsal"]["status"] == "Verified", observed
        assert sha(direct[3]) == report["initial_image_sha256"]
        direct[3].unlink()

    output = work / "staged"
    before = {path.name for path in work.iterdir()}
    chosen_budget = LARGE_COPY_MIB if large_copy is not None else 128
    stdout = succeeded(invoke(stage(output, max_copy_mib=chosen_budget), 850))
    assert {path.name for path in work.iterdir()} == before | {output.name}, "stage leaked private scratch paths"
    candidate = validate_bundle(args, output, stdout, old_build, frozen, old, receipt, code,
                                vars_file, baseline[0])
    if large_copy is not None:
        staged = json.loads(stdout)
        source = candidate["upgrade"]["verified_authority"]
        trial = staged["trial"]["verified_authority"]
        assert staged["inputs"]["max_copy_mib"] == source["max_copy_mib"] == trial["max_copy_mib"] == chosen_budget
        assert source["copied_bytes"] == large_copy, source
        assert 64 * MIB < trial["copied_bytes"] <= chosen_budget * MIB, trial
        print(f"PASS verified {large_copy} source bytes including WAL and {trial['copied_bytes']} migrated bytes",
              flush=True)
    assert digest_map(frozen) == frozen
    bundle = digest_map(output.iterdir())

    updated_boot = boot_view(work, output / "candidate.raw", vars_file, "candidate-boot",
                                 operator, admin, candidate, baseline, token)
    prior_boot = boot_view(work, old, vars_file, "previous-boot",
                               operator, admin, old_build, baseline, token)
    assert updated_boot != prior_boot
    assert digest_map(frozen) == frozen
    assert digest_map(bundle) == bundle
    print("PASS staged native update, independent candidate and original OVMF boots, and durable Core facts")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--previous-installer", type=Path, default=REPO / "images/native/create.py")
    for option in ("--kernel-iso", "--previous-archive", "--archive"):
        parser.add_argument(option, type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--expected-source-format", type=int)
    parser.add_argument("--expected-target-format", type=int)
    parser.add_argument("--large-authority", action="store_true", help="prove both build paths with a database and WAL over 64 MiB")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-staged-update-") as directory:
        exercise(args, Path(directory).resolve())


if __name__ == "__main__":
    main()
