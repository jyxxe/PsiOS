"""Interrupt and recover an exact managed deployment using real native files and QEMU."""

from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import select
import signal
import stat
import subprocess
import sys
import time
import uuid

from native_activation import JOURNAL, MATERIALIZED, assert_private, child_fd, no_qemu, rejected
from native_disk import sha
from native_selection import PLAN, RECORD, Selection, checked_material, entries, selection_bytes, snapshot, wrong_digest
from native_staged_update import succeeded


COPY_RECORD = "selection-copy.json"
BOOT_PROOF = "boot-proof.json"


@contextmanager
def traced_controller(command, trace, options):
    # -D leaves the Python selector as the direct child. Trace only that
    # selector so QEMU or qemu-img syscalls cannot consume a fault occurrence.
    invocation = ["strace", "-D", "-yy", "-o", trace, *options, "--", *command]
    with subprocess.Popen(invocation, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, start_new_session=True) as controller:
        try:
            yield controller
        finally:
            if controller.poll() is None:
                os.killpg(controller.pid, signal.SIGKILL)
                controller.wait(15)


def wait_for(controller, boundary, observe, timeout):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if controller.poll() is not None:
            stdout, stderr = controller.communicate(timeout=15)
            raise AssertionError(f"selector exited before {boundary}: {stdout[-1000:]!r} {stderr[-1200:]!r}")
        observed = observe()
        if observed is not None:
            return observed
        time.sleep(0.01)
    raise AssertionError(f"selector did not reach {boundary}")


def kill_controller(controller):
    assert os.path.samefile(f"/proc/{controller.pid}/exe", sys.executable), controller.pid
    controller.kill()
    assert controller.wait(15) == -signal.SIGKILL
    controller.communicate(timeout=15)


def optional_record(path):
    try:
        return json.loads(path.read_bytes())
    except FileNotFoundError:
        return None


def discard_command(fixture, state, approved):
    return [*fixture.tool, "discard-staging", "--state-dir", state, "--plan-digest", approved]


def exact_partial_copy(state, approved, generation):
    marker = optional_record(state / COPY_RECORD)
    if marker is None or marker["status"] != "Copying":
        return None
    assert marker["format"] == 1 and marker["plan_digest"] == approved, marker
    assert marker["generation"] == generation, marker
    name = marker["name"]
    assert re.fullmatch(r"\.selection-copy-[0-9a-f]{32}", name), marker
    staging = state / name
    try:
        info = staging.lstat()
        raw = (staging / "managed.raw").lstat()
    except FileNotFoundError:
        return None
    assert stat.S_ISDIR(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o700, info
    assert info.st_uid == os.geteuid() and (info.st_dev, info.st_ino) == (marker["device"], marker["inode"]), marker
    if 0 < raw.st_size < marker["size_bytes"]:
        return marker, staging
    return None


def recover_adoption(work, fixture, state, plan, activation):
    """Discard an interrupted, partially written selection copy by its exact approved identity."""
    approved = plan["plan_digest"]
    generation = plan["plan"]["generation"]
    published = state / f"generation-{generation}"
    assert not published.exists() and not (state / COPY_RECORD).exists()
    previous = selection_bytes(state)
    frozen_plan = snapshot([state / PLAN])

    def trace(step):
        return work / f"{state.name}-adoption-recovery-{step}.strace"

    with traced_controller(fixture.adopt(state, approved), trace("cut"), [
        "-e", "trace=write,renameat2", "-e", "inject=write:delay_exit=250ms:when=3+1",
    ]) as controller:
        marker, staging = wait_for(controller, "the exact partial raw copy",
                                   lambda: exact_partial_copy(state, approved, generation), 200)
        kill_controller(controller)

    assert selection_bytes(state) == previous and not published.exists()
    marker_path = state / COPY_RECORD
    assert_private(marker_path)
    assert optional_record(marker_path) == marker
    assert marker["size_bytes"] == (fixture.activation / "managed.raw").stat().st_size
    raw = staging / "managed.raw"
    assert_private(raw)
    assert 0 < raw.stat().st_size < marker["size_bytes"], raw.stat()
    assert entries(staging) == {"managed.raw"}, entries(staging)
    retained = snapshot([marker_path, raw])

    report = no_qemu(fixture.inspect(state), trace("inspect"))
    assert report.returncode != 0 and not report.stderr, report
    visible = json.loads(report.stdout)
    assert visible["status"] == "RecoveryRequired" and visible["staging"] == marker, visible
    rejected(no_qemu(fixture.adopt(state, approved), trace("retry")), r"copy|staging|recover|discard")
    rejected(no_qemu(discard_command(fixture, state, wrong_digest(approved)), trace("wrong-plan")),
             r"approval|digest|exact|copy")
    assert snapshot(retained) == retained

    foreign = staging / "unapproved-child"
    foreign.write_bytes(b"unrelated bytes that a staging cleanup must preserve\n")
    foreign.chmod(0o600)
    foreign_bytes = snapshot([foreign])
    rejected(no_qemu(discard_command(fixture, state, approved), trace("foreign-child")),
             r"unrecognized|foreign|unexpected|unsupported|not approved")
    assert snapshot(retained) == retained and snapshot(foreign_bytes) == foreign_bytes
    foreign.unlink()

    held = work / f"{state.name}-held-original-staging"
    staging.rename(held)
    staging.mkdir(mode=0o700)
    assert staging.stat().st_ino != marker["inode"]
    (held / raw.name).rename(raw)
    try:
        rejected(no_qemu(discard_command(fixture, state, approved), trace("replaced-inode")),
                 r"inode|identity|changed|exact|staging")
        assert snapshot(retained) == retained and held.is_dir()
    finally:
        if raw.exists():
            raw.rename(held / raw.name)
        if staging.exists():
            staging.rmdir()
        held.rename(staging)

    discarded = json.loads(succeeded(no_qemu(discard_command(fixture, state, approved), trace("discard"))))
    assert discarded["status"] == "Discarded", discarded
    assert discarded["plan_digest"] == approved and discarded["generation"] == generation, discarded
    assert not staging.exists() and not marker_path.exists() and not published.exists()
    assert selection_bytes(state) == previous and snapshot(frozen_plan) == frozen_plan
    replay = json.loads(succeeded(no_qemu(discard_command(fixture, state, approved), trace("discard-replay"))))
    assert replay == {**discarded, "status": "AlreadyDiscarded"}, replay
    assert snapshot(frozen_plan) == frozen_plan
    return fixture.adopted(state, plan, activation)


def synced_proof(trace):
    """Observe the successful receipt rename and its directory sync before killing the publisher."""
    try:
        trace_text = trace.read_text(errors="replace")
    except FileNotFoundError:
        return False
    publish = re.search(r'renameat\([^,]+, "[^"\n]+", (\d+)<[^>\n]+>, "boot-proof\.json"\)\s+= 0', trace_text)
    if publish is None:
        return False
    return re.search(rf'fsync\({publish[1]}<[^>\n]+>\)\s+= 0', trace_text[publish.end():]) is not None


def stop_after_durable_proof(fixture, state, selected, trace):
    directory = state / f"generation-{selected['generation']}"
    proof_path = directory / BOOT_PROOF
    assert not proof_path.exists()
    descriptor = None

    def observe():
        nonlocal descriptor
        issued = optional_record(state / RECORD)
        if issued is None or issued["status"] != "BootIssued":
            return None
        assert issued["selection_digest"] == selected["selection_digest"], issued
        if descriptor is None:
            child = child_fd(controller.pid)
            if child is not None:
                pid, descriptor, args = child
                assert b"-smp" in args and args[args.index(b"-smp") + 1] == b"1", args
                assert b"-nic" in args and args[args.index(b"-nic") + 1] == b"none", args
                expected = {(file.st_dev, file.st_ino) for file in ((directory / name).stat() for name in MATERIALIZED)}
                inherited = set()
                for handle in Path(f"/proc/{pid}/fd").iterdir():
                    try:
                        opened = handle.stat()
                    except FileNotFoundError:
                        continue
                    inherited.add((opened.st_dev, opened.st_ino))
                assert expected <= inherited, "QEMU did not inherit the selected boot materials"
        record = optional_record(proof_path)
        if record is None or not synced_proof(trace):
            return None
        assert descriptor is not None, "selector wrote proof without observed QEMU startup"
        assert select.select([descriptor], [], [], 0)[0], "QEMU still running after clean boot proof publication"
        return record, issued

    try:
        with traced_controller(fixture.boot(state, selected["selection_digest"]), trace, [
            "-e", "trace=renameat,fsync", "-e", "inject=renameat:delay_enter=4000ms:when=3",
        ]) as controller:
            sealed, issued = wait_for(controller, "the fsynced QEMU proof before pointer publication", observe, 285)
            boundary = selection_bytes(state)
            assert json.loads(boundary) == issued
            kill_controller(controller)
        assert selection_bytes(state) == boundary and optional_record(proof_path) == sealed
        assert_private(proof_path)
        return sealed, boundary
    finally:
        if descriptor is not None:
            try:
                if not select.select([descriptor], [], [], 0)[0]:
                    try:
                        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    assert select.select([descriptor], [], [], 15)[0], "test cleanup left QEMU alive"
            finally:
                os.close(descriptor)


def recover_boot(work, fixture, state, selected):
    """Publish a completed selected boot by its exact persisted proof without starting QEMU."""
    generation = state / f"generation-{selected['generation']}"
    previous = selected["selection_digest"]
    proof_path = generation / BOOT_PROOF
    original = snapshot(path for path in generation.iterdir() if path.name not in MATERIALIZED)

    def trace(step):
        return work / f"{state.name}-boot-recovery-{step}.strace"

    sealed, boundary = stop_after_durable_proof(fixture, state, selected, trace("cut"))
    verified = sealed["settled"]
    assert sealed["format"] == 1 and sealed["issued"] == json.loads(boundary), sealed
    attempt = uuid.UUID(sealed["issued"]["boot_attempt_id"])
    assert attempt.int != 0 and verified["boot_attempt_id"] == str(attempt), sealed
    assert verified["status"] == "Selected" and verified["generation"] == selected["generation"], verified
    assert verified["boot_generation"] == selected["boot_generation"] + 1, verified
    assert verified["selection_digest"] != previous and verified["adoption_plan_digest"] == selected["adoption_plan_digest"]
    assert verified["deployment"] == selected["deployment"] and verified["rollback_mode"] == "ManualRecoveryOnly"
    boot_id = verified["proof"]["boot_id"]
    assert uuid.UUID(boot_id).int != 0 and boot_id != selected["proof"]["boot_id"], verified
    assert verified["proof"] == {**selected["proof"], "boot_id": boot_id}, verified
    checked_material(generation, verified)
    assert snapshot(original) == original
    approved_proof = sha(proof_path)
    proof_bytes = snapshot([proof_path])
    assert fixture.secret.read_bytes().strip() not in proof_path.read_bytes()

    def reconcile(selection=previous, proof=approved_proof):
        return [*fixture.tool, "reconcile-boot", "--state-dir", state, "--selection-digest", selection,
                "--boot-proof-sha256", proof]

    outcome = no_qemu(fixture.inspect(state), trace("inspect"))
    assert outcome.returncode != 0 and not outcome.stderr, outcome
    visible = json.loads(outcome.stdout)
    assert visible["status"] == "RecoveryRequired" and visible["selection_digest"] == previous, visible
    assert visible["boot_proof_sha256"] == approved_proof, visible
    rejected(no_qemu(fixture.boot(state, previous), trace("duplicate-boot")), r"issued|inspect|recover|unresolved")
    rejected(no_qemu(reconcile(selection=wrong_digest(previous)), trace("wrong-selection")), r"digest|approval|exact")
    rejected(no_qemu(reconcile(proof=wrong_digest(approved_proof)), trace("wrong-proof")), r"digest|approval|exact")
    assert selection_bytes(state) == boundary and snapshot(proof_bytes) == proof_bytes

    variables = generation / "managed-efi-vars.fd"
    with variables.open("r+b", buffering=0) as stream:
        byte = stream.read(1)
        assert len(byte) == 1
        stream.seek(0)
        assert stream.write(bytes([byte[0] ^ 1])) == 1
        os.fsync(stream.fileno())
        try:
            rejected(no_qemu(reconcile(), trace("changed-material")), r"digest|bytes|changed|firmware|material")
            assert selection_bytes(state) == boundary and snapshot(proof_bytes) == proof_bytes
        finally:
            stream.seek(0)
            assert stream.write(byte) == 1
            os.fsync(stream.fileno())

    retained = snapshot(generation.iterdir())
    recovered = json.loads(succeeded(no_qemu(reconcile(), trace("reconcile"))))
    assert recovered == verified, recovered
    settled = snapshot([state / RECORD])
    assert json.loads(selection_bytes(state)) == verified
    assert snapshot(retained) == retained
    assert json.loads(succeeded(no_qemu(reconcile(), trace("replay")))) == recovered
    assert json.loads(succeeded(no_qemu(fixture.inspect(state), trace("verified")))) == recovered
    assert snapshot(settled) == settled and snapshot(retained) == retained
    print("PASS exact managed boot proof survived the controller and reconciled without launching QEMU")
    return recovered


def exercise_selection_recovery(work: Path, program: Path, activation_dir: Path, secret: Path):
    """Use an existing verified activation to cover partial adoption and completed-proof recovery."""
    activation = json.loads((activation_dir / JOURNAL).read_bytes())
    assert activation["status"] == "BootVerified", activation
    source = snapshot(activation_dir / name for name in MATERIALIZED | {JOURNAL})
    fixture = Selection(program, activation_dir, secret)
    state = work / "selection-recovery"
    state.mkdir(mode=0o700)
    plan = fixture.planned(state)
    selected = recover_adoption(work, fixture, state, plan, activation)
    print("PASS exact discard of an interrupted partial managed adoption and approved re-adoption")
    recovered = recover_boot(work, fixture, state, selected)
    assert snapshot(source) == source
    return state / f"generation-{recovered['generation']}", recovered
