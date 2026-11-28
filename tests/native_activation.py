#!/usr/bin/env python3
"""Exercise real approvable staged native activation and a witnessed lost boot receipt."""
import argparse
import json
import os
from pathlib import Path
import re
import secrets
import select
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid

from native_disk import (
    REPO, boot, check_clean_shutdown, extract_partition, password, replace_partition, sha,
)
from native_staged_update import digest_map, durable_fact, invoke, succeeded, validate_bundle
from native_upgrade import shutdown


JOURNAL = "operation.json"
MATERIALIZED = {"managed.raw", "managed-ovmf-code.fd", "managed-efi-vars.fd"}


def rejected(result, message):
    assert result.returncode != 0, result.stdout[-1500:]
    assert re.search(message, result.stderr + result.stdout, re.I), (result.stdout[-1500:], result.stderr[-1500:])


def assert_private(path):
    info = path.lstat()
    assert stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid(), path
    assert stat.S_IMODE(info.st_mode) == 0o600, path
    assert info.st_nlink == 1, path


def no_qemu(command, trace, timeout=80):
    result = invoke(["strace", "-f", "-e", "trace=execve,execveat", "-o", trace, "--", *command], timeout)
    calls = [line for line in trace.read_text().splitlines()
             if re.search(r'\bexecve(?:at)?\([^"]*"[^"]*qemu-system-x86_64"', line)]
    assert not calls, calls
    return result


def copy_disk(source, destination):
    succeeded(invoke(["qemu-img", "convert", "-q", "-m", "1", "-f", "raw", "-O", "raw",
                      source, destination], 150))


class Activation:
    def __init__(self, program, stage, source_disk, code, vars_file, secret):
        self.tool = [sys.executable, "-B", str(program)]
        self.stage = stage
        self.stage_digest = sha(stage / "staged-update.json")
        self.source = source_disk
        self.code = code
        self.variables = vars_file
        self.secret = secret

    def plan_command(self, state, *, source=None, stage_digest=None):
        return [*self.tool, "plan", "--state-dir", state, "--stage-dir", self.stage,
                "--stage-receipt-sha256", stage_digest or self.stage_digest,
                "--source-disk", source or self.source, "--ovmf-code", self.code,
                "--ovmf-vars", self.variables]

    def apply(self, state, plan_digest):
        return [*self.tool, "activate", "--state-dir", state, "--plan-digest", plan_digest,
                "--root-console-password-file", self.secret]

    def inspect(self, state):
        return [*self.tool, "inspect", "--state-dir", state]

    def planned(self, state, *, source=None):
        state.mkdir(mode=0o700)
        report = json.loads(succeeded(invoke(self.plan_command(state, source=source), 130)))
        assert report["status"] == "BootPending", report
        assert re.fullmatch(r"[0-9a-f]{64}", report["plan_digest"]), report
        assert report["planned_at"] and "boot_issued_at" not in report and "proof" not in report, report
        assert json.loads((state / JOURNAL).read_bytes()) == report, report
        assert_private(state / JOURNAL)
        assert {path.name for path in state.iterdir()} == MATERIALIZED | {JOURNAL}
        for name in MATERIALIZED:
            assert_private(state / name)
        return report


def child_fd(controller):
    try:
        children = Path(f"/proc/{controller}/task/{controller}/children").read_text().split()
    except FileNotFoundError:
        return None
    for item in children:
        pid = int(item)
        try:
            pidfd = os.pidfd_open(pid)
        except ProcessLookupError:
            continue
        try:
            try:
                executable = os.readlink(f"/proc/{pid}/exe")
                raw_cmdline = Path(f"/proc/{pid}/cmdline").read_bytes()
            except (FileNotFoundError, ProcessLookupError):
                continue
            # Linux can expose the new executable before its argument memory
            # is readable. An exiting child can also have an empty cmdline.
            if Path(executable).name != "qemu-system-x86_64" or not raw_cmdline.endswith(b"\0"):
                continue
            cmdline = raw_cmdline[:-1].split(b"\0")
            if Path(os.fsdecode(cmdline[0])).name != "qemu-system-x86_64" or select.select([pidfd], [], [], 0)[0]:
                continue
            return pid, os.dup(pidfd), cmdline
        finally:
            os.close(pidfd)
    return None


def stop_after_launch(command, state, fenced_source):
    witness = state / JOURNAL
    descriptor = None
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          stdin=subprocess.DEVNULL, start_new_session=True) as controller:
        try:
            deadline = time.monotonic() + 200
            while time.monotonic() < deadline:
                if controller.poll() is not None:
                    stdout, stderr = controller.communicate()
                    raise AssertionError(f"activation exited before its crash boundary: {stdout[-1024:]!r} {stderr[-1024:]!r}")
                report = json.loads(witness.read_bytes())
                if report["status"] == "BootIssued":
                    child = child_fd(controller.pid)
                    if child is not None:
                        pid, descriptor, cmdline = child
                        assert b"-smp" in cmdline and cmdline[cmdline.index(b"-smp") + 1] == b"1", cmdline
                        assert b"-nic" in cmdline and cmdline[cmdline.index(b"-nic") + 1] == b"none", cmdline
                        signal.pidfd_send_signal(descriptor, signal.SIGSTOP)
                        break
                assert report["status"] in ("BootPending", "BootIssued"), report
                time.sleep(0.025)
            if descriptor is None:
                raise AssertionError("activation never started a real QEMU child after BootIssued")
            boundary = witness.read_bytes()
            report = json.loads(boundary)
            assert report["status"] == "BootIssued" and report["boot_issued_at"] and "proof" not in report, report
            options = f"driver=raw,file.driver=file,file.filename={fenced_source},file.locking=on"
            compare = ["qemu-img", "compare", "-q", "--image-opts", options, options]
            rejected(invoke(compare, 30), r"lock")
            controller.kill()
            assert controller.wait(15) == -signal.SIGKILL
            assert select.select([descriptor], [], [], 15)[0], f"controller left its QEMU child {pid} alive"
            assert witness.read_bytes() == boundary
            succeeded(invoke(compare, 30))
            return boundary
        finally:
            if controller.poll() is None:
                os.killpg(controller.pid, signal.SIGKILL)
                controller.wait(15)
            if descriptor is not None:
                if not select.select([descriptor], [], [], 0)[0]:
                    try:
                        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    assert select.select([descriptor], [], [], 15)[0], "test cleanup left QEMU alive"
                os.close(descriptor)


def verify_guest(work, disk, code, variables, operator, admin, build, prior, token):
    with tempfile.TemporaryDirectory(dir=work, prefix="activation-boot-view-") as path:
        directory = Path(path)
        shutil.copyfile(code, directory / "code.fd")
        vars_file = directory / "vars.fd"
        shutil.copyfile(variables, vars_file)
        overlay = directory / "checked.qcow2"
        succeeded(invoke(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw",
                          "-b", disk, overlay], 30))
        with boot(directory, overlay, vars_file=vars_file.name, disk_format="qcow2", vcpus=1) as console:
            console.login("psios", operator)
            observed = durable_fact(console)
            assert observed[0] == prior[0], observed
            assert observed[1]["task_id"] == prior[1]["task_id"] and observed[1]["deduplicated"] is True, observed
            assert observed[2] == prior[2] and observed[4] == prior[4], observed
            assert observed[3] != prior[3], observed
            assert console.run("cat $HOME/native-activation-fact") == token
            assert console.run("cat /psios-source") == build["source_commit"]
            assert console.run("sha256sum /psios-deployment.json").split()[0] == build["deployment_manifest_sha256"]
            expansion = build.get("upgrade", {}).get("home_expansion")
            if expansion is not None:
                assert int(console.run("cat /sys/class/block/vda3/size")) == expansion["target_mib"] * 2048
                console.run("dd if=/dev/zero of=expanded-data bs=1M count=96 2>/dev/null; "
                            "test $(stat -c %s expanded-data) = 100663296")
            shutdown(console, admin)
    return observed[3]


def managed_bytes(state):
    return digest_map(state / name for name in MATERIALIZED)


def rejected_inputs(fixture, work, frozen):
    untrusted = work / "untrusted"
    untrusted.mkdir(mode=0o700)
    result = invoke(fixture.plan_command(untrusted, stage_digest="0" * 64), 60)
    rejected(result, r"staged update receipt SHA-256 mismatch")
    assert not list(untrusted.iterdir())

    # Keep the genuinely booted original intact while testing a change to an
    # independently planned byte-identical source.
    drift_source = work / "source-drift.raw"
    copy_disk(fixture.source, drift_source)
    drift_source.chmod(0o600)
    assert sha(drift_source) == frozen[fixture.source]
    drift = work / "drift-state"
    plan = fixture.planned(drift, source=drift_source)
    before = managed_bytes(drift)
    with drift_source.open("r+b", buffering=0) as disk:
        disk.seek(1024 * 1024)
        old = disk.read(1)
        assert len(old) == 1
        disk.seek(-1, os.SEEK_CUR)
        assert disk.write(bytes([old[0] ^ 1])) == 1
        os.fsync(disk.fileno())
    changed = sha(drift_source)
    assert changed != frozen[fixture.source]
    result = no_qemu(fixture.apply(drift, plan["plan_digest"]), work / "drift.strace")
    rejected(result, r"original disk identity .*changed|original disk.*(?:changed|mismatch)")
    assert json.loads((drift / JOURNAL).read_bytes()) == plan
    assert managed_bytes(drift) == before and sha(drift_source) == changed
    shutil.rmtree(drift)
    drift_source.unlink()


def interrupted_boot(fixture, work):
    state = work / "interrupted"
    plan = fixture.planned(state)
    command = fixture.apply(state, plan["plan_digest"])
    boundary = stop_after_launch(command, state, fixture.source)
    issued = json.loads(boundary)
    assert issued["plan"] == plan["plan"] and issued["plan_digest"] == plan["plan_digest"], issued
    observed = managed_bytes(state)
    report = no_qemu(fixture.inspect(state), work / "interrupted-inspect.strace")
    assert report.returncode != 0 and not report.stderr, report
    assert json.loads(report.stdout) == {**issued, "status": "RecoveryRequired",
                                         "reason": "issued boot has no verified receipt"}
    rejected(no_qemu(command, work / "interrupted-retry.strace"), r"boot was already issued")
    assert (state / JOURNAL).read_bytes() == boundary
    assert managed_bytes(state) == observed
    assert {path.name for path in state.iterdir()} == MATERIALIZED | {JOURNAL}
    shutil.rmtree(state)


def interrupted_plan(fixture, state, work):
    from native_selection_recovery import kill_controller, traced_controller, wait_for

    # Delay return from the second linkat after the kernel has linked the disk.
    # The firmware still exists only on anonymous descriptors at this boundary.
    state.mkdir(mode=0o700)
    trace = work / "activation-plan-cut.strace"
    command = fixture.plan_command(state)
    disk = state / "managed.raw"
    with traced_controller(command, trace, ["-e", "trace=linkat", "-e",
                           "inject=linkat:delay_exit=2000ms:when=2"]) as process:
        wait_for(process, "first published activation disk", lambda: True if disk.exists() else None, 130)
        kill_controller(process)
    marker = state / "preparing.json"
    assert {p.name for p in state.iterdir()} == {marker.name, disk.name}
    assert_private(marker)
    assert_private(disk)
    before = disk.stat()
    pending = json.loads(marker.read_bytes())
    inspected = json.loads(succeeded(no_qemu(fixture.inspect(state), work / "inspect-preparation.strace")))
    assert inspected == {**pending, "status": "Preparing"}
    rejected(no_qemu(fixture.apply(state, pending["plan_digest"]), work / "incomplete-apply.strace"),
             r"preparation is incomplete")
    wrong = work / "activation-alternate.raw"
    succeeded(invoke(["cp", "--sparse=always", "--reflink=auto", fixture.source, wrong], 90))
    wrong.chmod(0o600)
    rejected(invoke(fixture.plan_command(state, source=wrong), 90), r"different plan")
    wrong.unlink()
    # Refuse unrelated entries and damaged private material without replacing either.
    foreign = state / "operator-file"
    foreign.write_bytes(b"retain")
    rejected(invoke(command, 60), r"unrecognized contents")
    assert foreign.read_bytes() == b"retain"
    foreign.unlink()
    with disk.open("r+b") as changed:
        original = changed.read(1)
        changed.seek(0)
        changed.write(bytes([original[0] ^ 255]))
        changed.flush()
        rejected(invoke(command, 90), r"material differs")
        changed.seek(0)
        assert changed.read(1) == bytes([original[0] ^ 255])
        changed.seek(0)
        changed.write(original)
    assert json.loads(marker.read_bytes()) == pending
    resumed = json.loads(succeeded(invoke(command, 130)))
    assert resumed == pending and json.loads((state / JOURNAL).read_bytes()) == pending
    assert os.path.samestat(before, disk.stat()) and {p.name for p in state.iterdir()} == MATERIALIZED | {JOURNAL}
    assert json.loads(succeeded(invoke(command, 130))) == pending
    return resumed


def verified_boot(fixture, work, candidate, frozen, prior, admin):
    state = work / "verified"
    plan = interrupted_plan(fixture, state, work)
    staged_digest = candidate["initial_image_sha256"]
    assert managed_bytes(state) == {
        state / "managed.raw": staged_digest,
        state / "managed-ovmf-code.fd": frozen[fixture.code],
        state / "managed-efi-vars.fd": frozen[fixture.variables],
    }
    pending = (state / JOURNAL).read_bytes()
    wrong = ("a" if plan["plan_digest"][0] != "a" else "b") + plan["plan_digest"][1:]
    rejected(no_qemu(fixture.apply(state, wrong), work / "wrong-approval.strace"), r"approval.*exact")
    assert (state / JOURNAL).read_bytes() == pending

    command = fixture.apply(state, plan["plan_digest"])
    result = invoke(command, 300)
    report = json.loads(succeeded(result))
    assert report["format"] == 1 and report["status"] == "BootVerified", report
    assert report["rollback_mode"] == "ManualRecoveryOnly", report
    assert report["plan_digest"] == plan["plan_digest"] and report["plan"] == plan["plan"], report
    assert report["planned_at"] <= report["boot_issued_at"] <= report["boot_verified_at"], report
    proof = report["proof"]
    assert proof == {"node_id": prior[0], "deployment_manifest_sha256": candidate["deployment_manifest_sha256"],
                     "boot_id": proof["boot_id"]}, proof
    assert str(uuid.UUID(proof["boot_id"])) == proof["boot_id"] and proof["boot_id"] != prior[3], proof

    saved = (state / JOURNAL).read_bytes()
    assert json.loads(saved) == report and admin.encode() not in saved, report
    assert admin not in result.stdout + result.stderr
    assert_private(state / JOURNAL)
    for name in MATERIALIZED:
        assert_private(state / name)
    observed = managed_bytes(state)
    expected = {
        "disk": "managed.raw", "efi_code": "managed-ovmf-code.fd", "efi_vars": "managed-efi-vars.fd",
        "image_sha256": observed[state / "managed.raw"],
        "efi_code_sha256": observed[state / "managed-ovmf-code.fd"],
        "efi_vars_sha256": observed[state / "managed-efi-vars.fd"],
    }
    assert report["managed"] == expected and expected["efi_code_sha256"] == frozen[fixture.code], report
    for label, replay in (("inspect", fixture.inspect(state)), ("repeat", command)):
        returned = no_qemu(replay, work / f"verified-{label}.strace")
        assert json.loads(succeeded(returned)) == report
    assert (state / JOURNAL).read_bytes() == saved
    assert managed_bytes(state) == observed
    assert {path.name for path in state.iterdir()} == MATERIALIZED | {JOURNAL}
    return state, observed, proof["boot_id"]


def stage_native(args, stage, source, receipt, code, vars_file, frozen):
    growth = ("--expand-home-mib", "192") if args.expand_home else ()
    return succeeded(invoke([sys.executable, "-B", args.controller, stage,
        "--upgrade-from", source, "--previous-receipt", receipt, "--previous-receipt-sha256", frozen[receipt],
        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive),
        "--ovmf-code", code, "--ovmf-code-sha256", frozen[code],
        "--ovmf-vars", vars_file, "--ovmf-vars-sha256", frozen[vars_file], *growth], 850))


def verify_expansion_contract(activator, fixture, candidate):
    sys.path.insert(0, str(activator.resolve().parent))
    from activate_update import stage_contract

    assert candidate["upgrade"]["home_expansion"] == {"previous_mib": 64, "target_mib": 192}
    assert candidate["geometry_mib"]["home_mib"] == 192
    source_size = fixture.source.stat().st_size
    checked = stage_contract(fixture.stage, fixture.stage_digest, source_size)
    assert checked["size_bytes"] == (fixture.stage / "candidate.raw").stat().st_size
    assert source_size < checked["size_bytes"], (source_size, checked["size_bytes"])
    try:
        stage_contract(fixture.stage, fixture.stage_digest, checked["size_bytes"] + 512)
    except ValueError as error:
        assert "original disk size does not match the staged home expansion" in str(error), error
    else:
        raise AssertionError("activation accepted a source larger than the approved candidate")
    stage_path = fixture.stage / "staged-update.json"
    authentic = stage_path.read_bytes()
    changed = json.loads(authentic)
    assert changed["inputs"]["expand_home_mib"] == 192
    try:
        for requested in (193, 192.0):
            changed["inputs"]["expand_home_mib"] = requested
            stage_path.write_text(json.dumps(changed))
            try:
                stage_contract(fixture.stage, sha(stage_path), source_size)
            except ValueError as error:
                assert "home expansion does not match the candidate geometry" in str(error), error
            else:
                raise AssertionError("activation accepted invalid home expansion evidence")
    finally:
        stage_path.write_bytes(authentic)


def verify_expanded_activation(fixture, work, selector, candidate, prior):
    state = work / "expanded-activation"
    plan = fixture.planned(state)
    evidence = plan["plan"]
    assert evidence["source_identity"][2] == fixture.source.stat().st_size
    assert evidence["size_bytes"] == (fixture.stage / "candidate.raw").stat().st_size
    report = json.loads(succeeded(invoke(fixture.apply(state, plan["plan_digest"]), 300)))
    assert report["status"] == "BootVerified" and report["proof"]["node_id"] == prior[0], report
    assert report["proof"]["deployment_manifest_sha256"] == candidate["deployment_manifest_sha256"], report
    selected_state = work / "expanded-selection"
    selected_state.mkdir(mode=0o700)
    select = [sys.executable, "-B", selector]
    adoption = json.loads(succeeded(invoke([*select, "plan", "--state-dir", selected_state,
        "--activation-dir", state, "--activation-receipt-sha256", sha(state / JOURNAL),
        "--expected-generation", "0"], 130)))
    selected = json.loads(succeeded(invoke([*select, "adopt", "--state-dir", selected_state,
        "--plan-digest", adoption["plan_digest"]], 170)))
    assert selected["status"] == "Selected" and selected["proof"] == report["proof"], selected
    assert selected["deployment"]["source_identity"] == evidence["source_identity"], selected
    assert selected["deployment"]["size_bytes"] == evidence["size_bytes"], selected
    return state


def refuse_unclean_shutdown(args, work, old, old_build, receipt, code, vars_file, secret, node_id):
    # Keep the original booted disk untouched. Reset just the shutdown action
    # in an independently staged, otherwise identical safe fixture copy.
    source = work / "without-shutdown-hook.raw"
    copy_disk(old, source)
    source.chmod(0o600)
    table = json.loads(subprocess.check_output(["sfdisk", "--json", source], timeout=15))["partitiontable"]
    assert table["sectorsize"] == 512, table
    parts = table["partitions"]
    assert len(parts) == 3 and all(part["start"] % 2048 == 0 and part["size"] % 2048 == 0
                                   for part in parts), table
    starts = tuple(part["start"] // 2048 for part in parts[1:])
    state_partition = work / "without-shutdown-hook.ext4"
    extract_partition(source, state_partition, starts[0], parts[1]["size"] // 2048)
    inittab = subprocess.check_output(["debugfs", "-R", "cat /etc/inittab", state_partition], text=True,
                                      stderr=subprocess.DEVNULL, timeout=15)
    hook = "::shutdown:/bin/sh /psios-shutdown"
    assert inittab.splitlines().count(hook) == 1, inittab
    modified = work / "without-shutdown-hook.inittab"
    previous = inittab.replace(hook, "::shutdown:/bin/busybox true")
    modified.write_text(previous)
    patch = (f"rm /etc/inittab\nwrite {modified.name} /etc/inittab\n"
             "set_inode_field /etc/inittab uid 0\nset_inode_field /etc/inittab gid 0\n"
             "set_inode_field /etc/inittab mode 0100644\n")
    subprocess.run(["debugfs", "-w", "-f", "-", state_partition], cwd=work, input=patch,
                   text=True, capture_output=True, check=True, timeout=20)
    assert subprocess.check_output(["debugfs", "-R", "cat /etc/inittab", state_partition], text=True,
                                   stderr=subprocess.DEVNULL, timeout=15) == previous
    replace_partition(source, state_partition, starts[0])
    state_partition.unlink()
    modified.unlink()
    check_clean_shutdown(source, starts=starts)
    with source.open("rb") as stable:
        os.fsync(stable.fileno())
    frozen = digest_map((source, receipt, code, vars_file))

    staged = work / "stage-without-shutdown-hook"
    report = stage_native(args, staged, source, receipt, code, vars_file, frozen)
    validate_bundle(args, staged, report, old_build, frozen, source, receipt, code, vars_file, node_id)
    check_clean_shutdown(staged / "candidate.raw", starts=starts)
    bundle = digest_map(staged.iterdir())
    state = work / "unclean-managed-activation"
    fixture = Activation(args.activator, staged, source, code, vars_file, secret)
    plan = fixture.planned(state)
    command = fixture.apply(state, plan["plan_digest"])
    # This error is downstream of the real authenticated console boot,
    # kernel Power down serial line and QEMU status 0.
    rejected(invoke(command, 300), r"managed candidate (state|home) ext4 was not cleanly shut down")
    try:
        check_clean_shutdown(state / "managed.raw", starts=starts)
    except AssertionError:
        pass
    else:
        raise AssertionError("guest without the native shutdown hook left no dirty ext4 evidence")

    boundary = (state / JOURNAL).read_bytes()
    issued = json.loads(boundary)
    assert issued["status"] == "BootIssued" and issued["plan"] == plan["plan"], issued
    assert "proof" not in issued and "boot_verified_at" not in issued, issued
    inspect = no_qemu(fixture.inspect(state), work / "unclean-inspect.strace")
    assert inspect.returncode != 0 and json.loads(inspect.stdout)["status"] == "RecoveryRequired", inspect
    stopped = managed_bytes(state)
    rejected(no_qemu(command, work / "unclean-replay.strace"), r"boot was already issued")
    assert (state / JOURNAL).read_bytes() == boundary and managed_bytes(state) == stopped
    if args.prior_activator is not None:
        check_legacy_adoption(args, work, staged, source, code, vars_file, secret, starts)
    assert digest_map(frozen) == frozen and digest_map(bundle) == bundle
    print("PASS actual native poweroff with dirty ext4 retains the issued boot for recovery")


def check_legacy_adoption(args, work, staged, source, code, vars_file, secret, starts):
    state = work / "legacy-unclean-activation"
    legacy = Activation(args.prior_activator, staged, source, code, vars_file, secret)
    plan = legacy.planned(state)
    report = json.loads(succeeded(invoke(legacy.apply(state, plan["plan_digest"]), 300)))
    assert report["status"] == "BootVerified", report
    try:
        check_clean_shutdown(state / "managed.raw", starts=starts)
    except AssertionError:
        pass
    else:
        raise AssertionError("legacy activation without shutdown hook did not leave dirty ext4 evidence")

    saved = {state / JOURNAL: sha(state / JOURNAL), **managed_bytes(state)}
    selection = work / "selection-legacy-unclean-activation"
    selection.mkdir(mode=0o700)
    command = [sys.executable, "-B", args.selector, "plan", "--state-dir", selection,
               "--activation-dir", state, "--activation-receipt-sha256", saved[state / JOURNAL],
               "--expected-generation", "0"]
    rejected(no_qemu(command, work / "unclean-legacy-selection.strace"),
             r"managed candidate (state|home) ext4 was not cleanly shut down")
    assert not (selection / "selection-plan.json").exists() and not (selection / "selection.json").exists()
    assert digest_map(saved) == saved
    print("PASS actual prior controller BootVerified receipt with dirty ext4 cannot be selected")


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    secret = work / "root-console"
    with os.fdopen(os.open(secret, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
        stream.write(admin + "\n")
    assert_private(secret)
    old = work / "before.raw"
    receipt = work / "before.json"
    code = work / "code.fd"
    vars_file = work / "before-vars.fd"
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", code)
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", vars_file)
    original = succeeded(invoke([sys.executable, "-B", args.previous_installer, old,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.previous_archive,
        "--core-sha256", sha(args.previous_archive), "--console-hash", operator_hash,
        "--root-console-hash", admin_hash], 330))
    receipt.write_text(original)
    old_build = json.loads(original)
    assert sha(old) == old_build["initial_image_sha256"], old_build
    token = secrets.token_hex(12)
    with boot(work, old, vars_file=vars_file.name, vcpus=1) as console:
        console.login("psios", operator)
        prior = durable_fact(console)
        console.run(f"printf '%s\\n' {shlex.quote(token)} > $HOME/native-activation-fact; sync")
        shutdown(console, admin)
    frozen = digest_map((old, receipt, code, vars_file))
    assert frozen[old] != old_build["initial_image_sha256"]

    if args.unclean_shutdown_only:
        refuse_unclean_shutdown(args, work, old, old_build, receipt, code, vars_file, secret, prior[0])
        assert digest_map(frozen) == frozen
        return

    stage = work / "staged"
    stdout = stage_native(args, stage, old, receipt, code, vars_file, frozen)
    candidate = validate_bundle(args, stage, stdout, old_build, frozen, old, receipt, code, vars_file, prior[0])
    bundle = digest_map(stage.iterdir())
    fixture = Activation(args.activator, stage, old, code, vars_file, secret)
    if args.expand_home:
        verify_expansion_contract(args.activator, fixture, candidate)
        if args.staging_only:
            verify_guest(work, stage / "candidate.raw", code, vars_file, operator, admin, candidate, prior, token)
            print("PASS isolated trial and candidate boot with verified expansion lineage")
        else:
            state = verify_expanded_activation(fixture, work, args.selector, candidate, prior)
            verify_guest(work, state / "managed.raw", state / "managed-ovmf-code.fd", state / "managed-efi-vars.fd",
                         operator, admin, candidate, prior, token)
            print("PASS activated and selected the expanded native home with the original Task")
        verify_guest(work, old, code, vars_file, operator, admin, old_build, prior, token)
        assert digest_map(frozen) == frozen and digest_map(bundle) == bundle
        print("PASS original home and source inputs remained independently bootable and unchanged")
        return
    rejected_inputs(fixture, work, frozen)
    interrupted_boot(fixture, work)
    state, managed, activated_boot = verified_boot(fixture, work, candidate, frozen, prior, admin)
    from native_selection import exercise_selection
    from native_selection_recovery import exercise_selection_recovery
    from native_selection_successor import exercise_successor
    selected_dir, selected = exercise_selection(work, args.selector, state, secret)
    successor_dir, successor, successor_build, successor_first_boot = exercise_successor(
        args, work, selected_dir, selected, secret)
    _, recovered = exercise_selection_recovery(work, args.selector, state, secret)
    if args.check_unclean_shutdown:
        refuse_unclean_shutdown(args, work, old, old_build, receipt, code, vars_file, secret, prior[0])
    secret.unlink()

    booted = verify_guest(work, successor_dir / "managed.raw", successor_dir / "managed-ovmf-code.fd",
                          successor_dir / "managed-efi-vars.fd", operator, admin, successor_build, prior, token)
    rebooted = verify_guest(work, old, code, vars_file, operator, admin, old_build, prior, token)
    assert len({prior[3], activated_boot, selected["proof"]["boot_id"], successor_first_boot,
                successor["proof"]["boot_id"], recovered["proof"]["boot_id"], booted, rebooted}) == 8
    assert digest_map(frozen) == frozen and digest_map(bundle) == bundle
    assert managed_bytes(state) == managed
    print("PASS native activation, selected deployment boot, crash fencing, original bytes and durable guest data")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--activator", type=Path, default=REPO / "images/native/activate_update.py")
    parser.add_argument("--selector", type=Path, default=REPO / "images/native/select_update.py")
    parser.add_argument("--successor-archive", type=Path)
    parser.add_argument("--expected-successor-format", type=int)
    parser.add_argument("--previous-installer", type=Path, default=REPO / "images/native/create.py")
    for name in ("--kernel-iso", "--previous-archive", "--archive"):
        parser.add_argument(name, type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--work-parent", type=Path)
    parser.add_argument("--expected-source-format", type=int)
    parser.add_argument("--expected-target-format", type=int)
    parser.add_argument("--prior-activator", type=Path)
    cases = parser.add_mutually_exclusive_group()
    cases.add_argument("--check-unclean-shutdown", action="store_true")
    cases.add_argument("--unclean-shutdown-only", action="store_true")
    cases.add_argument("--expand-home", action="store_true", help="stage and activate a 192 MiB home")
    parser.add_argument("--staging-only", action="store_true", help="verify trial and candidate without managed activation")
    args = parser.parse_args()
    if args.staging_only and not args.expand_home:
        parser.error("--staging-only requires --expand-home")
    if os.geteuid() == 0:
        parser.error("run the managed activation acceptance as an unprivileged Linux operator")
    with tempfile.TemporaryDirectory(dir=args.work_parent, prefix="psios-native-activation-") as directory:
        exercise(args, Path(directory).resolve())


if __name__ == "__main__":
    main()
