"""Exercise managed native selection and first use of the selected deployment."""
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import sys
import time
import uuid

from native_activation import JOURNAL, MATERIALIZED, assert_private, child_fd, copy_disk, no_qemu, rejected
from native_disk import sha
from native_staged_update import invoke, succeeded


RECORD = "selection.json"
PLAN = "selection-plan.json"


def wrong_digest(value):
    return ("a" if value[0] != "a" else "b") + value[1:]


def snapshot(paths):
    recorded = {}
    for path in paths:
        info = path.lstat()
        recorded[path] = (info.st_dev, info.st_ino, info.st_size, info.st_mode, info.st_nlink, sha(path))
    return recorded


def entries(directory):
    return {path.name for path in directory.iterdir()}


def selection_history(state, generation):
    top = (path for path in state.iterdir() if path.name != RECORD and path.is_file())
    objects = (path for path in generation.iterdir() if path.name not in MATERIALIZED and path.is_file())
    return snapshot((*top, *objects))


class Selection:
    def __init__(self, program, activation, secret):
        self.tool = [sys.executable, "-B", program]
        self.activation = activation
        self.activation_digest = sha(activation / JOURNAL)
        self.secret = secret

    def plan(self, state, generation, *, receipt_digest=None):
        return [*self.tool, "plan", "--state-dir", state, "--activation-dir", self.activation,
                "--activation-receipt-sha256", receipt_digest or self.activation_digest,
                "--expected-generation", str(generation)]

    def adopt(self, state, plan_digest):
        return [*self.tool, "adopt", "--state-dir", state, "--plan-digest", plan_digest]

    def inspect(self, state):
        return [*self.tool, "inspect", "--state-dir", state]

    def boot(self, state, selection_digest):
        return [*self.tool, "boot-selected", "--state-dir", state, "--selection-digest", selection_digest,
                "--root-console-password-file", self.secret]

    def planned(self, state):
        plan = json.loads(succeeded(invoke(self.plan(state, 0), 150)))
        assert plan["status"] == "Planned", plan
        assert re.fullmatch(r"[0-9a-f]{64}", plan["plan_digest"]), plan
        assert plan["plan"]["expected_generation"] == 0 and plan["plan"]["generation"] == 1, plan
        assert not (state / RECORD).exists()
        assert not (state / "generation-1").exists()
        assert_private(state / PLAN)
        return plan

    def adopted(self, state, plan, activation):
        report = json.loads(succeeded(invoke(self.adopt(state, plan["plan_digest"]), 170)))
        generation = plan["plan"]["generation"]
        assert report["status"] == "Selected" and report["generation"] == generation, report
        assert report["boot_generation"] == 0 and report["adoption_plan_digest"] == plan["plan_digest"], report
        assert report["rollback_mode"] == "ManualRecoveryOnly", report
        assert re.fullmatch(r"[0-9a-f]{64}", report["selection_digest"]), report
        assert report["proof"] == activation["proof"], report
        assert report["deployment"]["activation_receipt_sha256"] == self.activation_digest, report
        assert report["deployment"]["activation_plan_digest"] == activation["plan_digest"], report
        assert json.loads((state / RECORD).read_bytes()) == report
        assert_private(state / RECORD)
        materialized = state / f"generation-{generation}"
        checked_material(materialized, report)
        assert_private(materialized / "adoption.json")
        build = materialized / "candidate-build.json"
        original_build = Path(activation["plan"]["stage_dir"]) / build.name
        assert_private(build)
        assert not original_build.samefile(build) and sha(original_build) == sha(build)
        for name in MATERIALIZED:
            original = self.activation / name
            selected = materialized / name
            assert not original.samefile(selected), name
            assert sha(original) == sha(selected), name
        return report


def checked_material(directory, report):
    expected = report["current"]
    assert expected["boot_id"] == report["proof"]["boot_id"], report
    assert directory.is_dir() and not directory.is_symlink()
    for name, field in (
        ("managed.raw", "image_sha256"),
        ("managed-ovmf-code.fd", "efi_code_sha256"),
        ("managed-efi-vars.fd", "efi_vars_sha256"),
    ):
        path = directory / name
        assert_private(path)
        assert sha(path) == expected[field], (name, expected)


def selection_bytes(state):
    try:
        return (state / RECORD).read_bytes()
    except FileNotFoundError:
        return None


def stop_after_publication(work: Path, fixture: Selection, state: Path, plan: dict, activation: dict):
    """Kill an adoption after its generation rename, then recover only the exact approved pointer."""
    expected = plan["plan"]["expected_generation"]
    generation = plan["plan"]["generation"]
    published = state / f"generation-{generation}"

    def trace_file(step):
        return work / f"{state.name}-generation-{generation}-publication-{step}.strace"

    trace = trace_file("cut")
    previous = selection_bytes(state)
    prior = json.loads(previous) if previous is not None else None
    assert (prior["generation"] if prior else 0) == expected, prior
    assert not published.exists()
    frozen_plan = snapshot([state / PLAN])
    command = [
        "strace", "-D", "-f", "-o", trace, "-e", "trace=renameat2",
        "-e", "inject=renameat2:delay_exit=1200ms:when=1", "--",
        *fixture.adopt(state, plan["plan_digest"]),
    ]
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          start_new_session=True) as selector:
        try:
            deadline = time.monotonic() + 200
            while not published.is_dir():
                if selector.poll() is not None:
                    stdout, stderr = selector.communicate()
                    raise AssertionError(
                        f"adoption exited before publishing the generation: {stdout[-800:]!r} {stderr[-1000:]!r}"
                    )
                assert time.monotonic() < deadline, "adoption did not publish the approved generation"
                time.sleep(0.005)
            assert os.path.samefile(f"/proc/{selector.pid}/exe", sys.executable), selector.pid
            assert selection_bytes(state) == previous, "selection pointer advanced before the crash boundary"
            selector.kill()
            assert selector.wait(15) == -signal.SIGKILL
            selector.communicate(timeout=15)
        finally:
            if selector.poll() is None:
                os.killpg(selector.pid, signal.SIGKILL)
                selector.wait(15)

    assert selection_bytes(state) == previous
    until = time.monotonic() + 5
    while True:
        trace_output = trace.read_text(errors="replace")
        if f'"{published.name}"' in trace_output and "RENAME_NOREPLACE" in trace_output:
            break
        assert time.monotonic() < until, trace_output[-1500:]
        time.sleep(0.01)
    directory = published.stat()
    retained = snapshot(published.iterdir())
    assert all(published / name in retained for name in MATERIALIZED)
    marker = published / "adoption.json"
    assert_private(marker)
    adopting = json.loads(marker.read_bytes())
    assert adopting["status"] == "Selected" and adopting["generation"] == generation, adopting
    assert adopting["adoption_plan_digest"] == plan["plan_digest"], adopting
    if prior is not None:
        assert (published / "previous-selection.json").read_bytes() == previous

    reported = no_qemu(fixture.inspect(state), trace_file("inspect"))
    assert reported.returncode != 0 and not reported.stderr, reported
    report = json.loads(reported.stdout)
    assert report["status"] == "RecoveryRequired" and report["generation"] == expected, report
    assert report["pending_generation"] == generation, report
    assert "published generation awaits exact adoption" in report["reason"], report
    if prior is not None:
        assert report["selection_digest"] == prior["selection_digest"], report

    rejected(no_qemu(fixture.adopt(state, wrong_digest(plan["plan_digest"])),
                     trace_file("wrong-adoption")), r"approval|digest|exact")
    candidate_digests = {adopting["selection_digest"]}
    if prior is not None:
        candidate_digests.add(prior["selection_digest"])
    for index, digest in enumerate(sorted(candidate_digests)):
        failed = no_qemu(fixture.boot(state, digest), trace_file(f"boot-{index}"))
        rejected(failed, r"no selected deployment|published generation|recovery|exact current selection digest")
    assert selection_bytes(state) == previous
    assert snapshot(published.iterdir()) == retained and snapshot(frozen_plan) == frozen_plan

    selected = fixture.adopted(state, plan, activation)
    assert selected == adopting, selected
    assert (state / RECORD).read_bytes() == marker.read_bytes()
    assert os.path.samestat(published.stat(), directory), "approved adoption replaced the published generation"
    assert snapshot(published.iterdir()) == retained and snapshot(frozen_plan) == frozen_plan
    return selected


def stopped_after_launch(command, state, generation):
    witness = state / RECORD
    descriptor = None
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          start_new_session=True) as controller:
        try:
            deadline = time.monotonic() + 200
            while time.monotonic() < deadline:
                if controller.poll() is not None:
                    stdout, stderr = controller.communicate()
                    raise AssertionError(
                        f"selected boot exited before its crash boundary: {stdout[-800:]!r} {stderr[-1000:]!r}"
                    )
                report = json.loads(witness.read_bytes())
                if report["status"] == "BootIssued":
                    child = child_fd(controller.pid)
                    if child is not None:
                        pid, descriptor, args = child
                        assert b"-smp" in args and args[args.index(b"-smp") + 1] == b"1", args
                        assert b"-nic" in args and args[args.index(b"-nic") + 1] == b"none", args
                        signal.pidfd_send_signal(descriptor, signal.SIGSTOP)
                        expected = {
                            (info.st_dev, info.st_ino)
                            for info in ((generation / name).stat() for name in MATERIALIZED)
                        }
                        inherited = set()
                        for fd in Path(f"/proc/{pid}/fd").iterdir():
                            try:
                                info = fd.stat()
                                inherited.add((info.st_dev, info.st_ino))
                            except FileNotFoundError:
                                pass
                        assert expected <= inherited, "QEMU did not inherit the exact selected deployment"
                        break
                assert report["status"] in ("Selected", "BootIssued"), report
                time.sleep(0.025)
            assert descriptor is not None, "selected boot did not launch QEMU after its durable issue"
            boundary = witness.read_bytes()
            assert json.loads(boundary)["status"] == "BootIssued"
            controller.kill()
            assert controller.wait(15) == -signal.SIGKILL
            assert select.select([descriptor], [], [], 15)[0], f"controller left live QEMU {pid}"
            assert witness.read_bytes() == boundary
            return boundary
        finally:
            if controller.poll() is None:
                os.killpg(controller.pid, signal.SIGKILL)
                controller.wait(15)
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


def reject_changed_activation(work, fixture):
    # Use a byte-identical private copy to prove the selector detects drift
    # between planning and adoption without changing the real activation.
    activation = work / "selection-activation-to-fence"
    activation.mkdir(mode=0o700)
    copy_disk(fixture.activation / "managed.raw", activation / "managed.raw")
    for name in sorted((MATERIALIZED | {JOURNAL}) - {"managed.raw"}):
        shutil.copyfile(fixture.activation / name, activation / name)
    for path in activation.iterdir():
        path.chmod(0o600)
        assert sha(path) == sha(fixture.activation / path.name)
    gated = Selection(fixture.tool[-1], activation, fixture.secret)
    state = work / "selection-changed-activation"
    state.mkdir(mode=0o700)
    plan = gated.planned(state)
    before = snapshot(path for path in state.iterdir() if path.is_file())
    names = entries(state)
    variables = activation / "managed-efi-vars.fd"
    with variables.open("r+b", buffering=0) as stream:
        original = stream.read(1)
        assert len(original) == 1
        stream.seek(0)
        assert stream.write(bytes([original[0] ^ 1])) == 1
        os.fsync(stream.fileno())
    changed = snapshot(activation.iterdir())
    assert changed[variables][-1] != sha(fixture.activation / variables.name)
    denied = no_qemu(gated.adopt(state, plan["plan_digest"]), work / "selection-activation-drift.strace")
    rejected(denied, r"digest|sha.?256|changed|differs|mismatch|activation.*drift")
    assert entries(state) == names and snapshot(before) == before
    assert snapshot(changed) == changed
    assert not (state / RECORD).exists() and not (state / "generation-1").exists()
    shutil.rmtree(state)
    shutil.rmtree(activation)


def exercise_interrupted_selection(work, fixture, activation):
    state = work / "selection-interrupted"
    state.mkdir(mode=0o700)
    plan = fixture.planned(state)
    initial = fixture.adopted(state, plan, activation)
    materialized = state / "generation-1"
    history = selection_history(state, materialized)
    command = fixture.boot(state, initial["selection_digest"])
    boundary = stopped_after_launch(command, state, materialized)
    issued = json.loads(boundary)
    assert issued["generation"] == initial["generation"] and issued["boot_generation"] == 0, issued
    assert snapshot(history) == history
    retained = snapshot(materialized.iterdir())
    assert all(materialized / name in retained for name in MATERIALIZED)
    before = snapshot(path for path in state.iterdir() if path.is_file())
    names = entries(state)

    result = no_qemu(fixture.inspect(state), work / "selection-interrupted-inspect.strace")
    assert result.returncode != 0 and not result.stderr, result
    report = json.loads(result.stdout)
    assert report["status"] == "RecoveryRequired" and report["generation"] == initial["generation"], report
    retries = {initial["selection_digest"], issued["selection_digest"]}
    for index, digest in enumerate(sorted(retries)):
        rejected(no_qemu(fixture.boot(state, digest), work / f"selection-interrupted-retry-{index}.strace"),
                 r"boot.*issued|recovery|unknown|unresolved")
    assert (state / RECORD).read_bytes() == boundary
    assert entries(state) == names and snapshot(before) == before
    assert snapshot(retained) == retained
    shutil.rmtree(state)


def exercise_selection(work: Path, program: Path, activation_dir: Path, secret: Path, *, interrupt=True):
    """Return the physically distinct selected generation and its verified boot receipt."""
    activation = json.loads((activation_dir / JOURNAL).read_bytes())
    assert activation["status"] == "BootVerified", activation
    frozen = snapshot(activation_dir / name for name in MATERIALIZED | {JOURNAL})
    fixture = Selection(program, activation_dir, secret)
    reject_changed_activation(work, fixture)
    if interrupt:
        exercise_interrupted_selection(work, fixture, activation)
    assert snapshot(frozen) == frozen

    state = work / "selection"
    state.mkdir(mode=0o700)
    rejected(no_qemu(fixture.plan(state, 1), work / "selection-wrong-generation.strace"), r"generation")
    rejected(no_qemu(fixture.plan(state, 0, receipt_digest=wrong_digest(fixture.activation_digest)),
                     work / "selection-wrong-activation-receipt.strace"), r"digest|sha.?256")
    assert not entries(state), state
    plan = fixture.planned(state)
    names = entries(state)
    planned = snapshot(path for path in state.iterdir() if path.is_file())
    rejected(no_qemu(fixture.adopt(state, wrong_digest(plan["plan_digest"])), work / "selection-wrong-plan.strace"),
             r"approval|digest|exact")
    assert entries(state) == names and snapshot(planned) == planned
    selected = (stop_after_publication(work, fixture, state, plan, activation) if interrupt
                else fixture.adopted(state, plan, activation))
    generation = state / "generation-1"
    material = snapshot(generation.iterdir())
    history = selection_history(state, generation)
    stored = (state / RECORD).read_bytes()
    stale = selected["selection_digest"]
    rejected(no_qemu(fixture.boot(state, wrong_digest(stale)), work / "selection-wrong-boot.strace"),
             r"digest|approval|exact")
    assert (state / RECORD).read_bytes() == stored and snapshot(material) == material

    result = invoke(fixture.boot(state, stale), 300)
    actual = json.loads(succeeded(result))
    assert actual["status"] == "Selected" and actual["generation"] == selected["generation"], actual
    assert actual["boot_generation"] == selected["boot_generation"] + 1, actual
    assert actual["adoption_plan_digest"] == selected["adoption_plan_digest"], actual
    assert actual["deployment"] == selected["deployment"], actual
    assert actual["rollback_mode"] == "ManualRecoveryOnly", actual
    assert actual["selection_digest"] != stale, actual
    assert re.fullmatch(r"[0-9a-f]{64}", actual["selection_digest"]), actual
    proof = actual["proof"]
    boot_identity = uuid.UUID(proof["boot_id"])
    assert str(boot_identity) == proof["boot_id"] and boot_identity.int != 0, proof
    assert proof["boot_id"] != activation["proof"]["boot_id"], proof
    assert proof == {**activation["proof"], "boot_id": proof["boot_id"]}, proof
    checked_material(generation, actual)
    assert actual["current"]["efi_code_sha256"] == selected["current"]["efi_code_sha256"], actual
    credential = secret.read_bytes().strip()
    assert credential not in (state / RECORD).read_bytes()
    assert credential not in (result.stdout + result.stderr).encode()
    assert snapshot(history) == history
    bytes_after = snapshot(generation.iterdir())
    state_after = snapshot(path for path in state.iterdir() if path.is_file())
    names = entries(state)
    for name in ("inspect", "same-adoption"):
        command = fixture.inspect(state) if name == "inspect" else fixture.adopt(state, plan["plan_digest"])
        replay = no_qemu(command, work / f"selection-{name}.strace")
        assert json.loads(succeeded(replay)) == actual
    rejected(no_qemu(fixture.boot(state, stale), work / "selection-stale-boot.strace"), r"digest|approval|exact")
    rejected(no_qemu(fixture.plan(state, 0), work / "selection-stale-generation.strace"), r"generation")
    rejected(no_qemu(fixture.plan(state, 1), work / "selection-stale-lineage.strace"),
             r"does not descend from the current selected disk")
    assert entries(state) == names and snapshot(state_after) == state_after
    assert snapshot(bytes_after) == bytes_after and snapshot(frozen) == frozen
    return generation, actual
