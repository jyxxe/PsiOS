#!/usr/bin/env python3
"""Exercise the native blank-device installer on disposable Linux loop media and OVMF."""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import time

from native_disk import REPO, boot, check_state, password, sha


LOOP_GET_STATUS64 = 0x4C05
LOOP_CLR_FD = 0x4C01
LOOP_INFO64_SIZE = 232
FIXTURE_TIMEOUT = 150
OUTER_MOUNT_NS = "PSIOS_NATIVE_INSTALL_FIXTURE_OUTER_MOUNT_NAMESPACE"
MANAGED_SYSCALLS = "pwrite64,pwritev,pwritev2,write,fsync"


def command(args, timeout=FIXTURE_TIMEOUT):
    # A timed-out tracer must not leave a writer running while its loop is released.
    with subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                          start_new_session=True) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=10)
            raise
        return subprocess.CompletedProcess(args, process.returncode, stdout, stderr)


def checked(result):
    if result.returncode:
        raise AssertionError(f"fixture command failed ({result.returncode}): {result.stderr[-1600:]}")
    return result.stdout


def sparse(work, name, length):
    backing = work / name
    with backing.open("xb") as handle:
        os.fchmod(handle.fileno(), 0o600)
        handle.truncate(length)
    return backing


class OwnedLoop:
    """Hold the exact backing inode on an open block FD for validation and cleanup."""

    def __init__(self, backing, *, reuse=None, sector_size=None):
        file = backing.lstat()
        assert stat.S_ISREG(file.st_mode) and file.st_nlink == 1, file
        assert file.st_uid == os.geteuid() and stat.S_IMODE(file.st_mode) == 0o600, file
        if reuse is not None:
            assert re.fullmatch(r"/dev/loop[0-9]+", str(reuse)), reuse
        setup = ["losetup"]
        if sector_size is not None:
            setup += ["--sector-size", str(sector_size)]
        if reuse is None:
            setup += ["--find", "--show", "--nooverlap", "--", str(backing)]
            name = checked(command(setup)).strip()
        else:
            checked(command([*setup, "--", str(reuse), str(backing)]))
            name = str(reuse)
        assert re.fullmatch(r"/dev/loop[0-9]+", name), name
        self.path = Path(name)
        self.backing = backing
        self.file = file
        self.fd = os.open(self.path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        try:
            self.check()
        except BaseException:
            os.close(self.fd)
            raise

    def binding(self):
        device = os.fstat(self.fd)
        assert stat.S_ISBLK(device.st_mode) and os.major(device.st_rdev) == 7, device
        assert self.path.name == f"loop{os.minor(device.st_rdev)}", device
        raw = bytearray(LOOP_INFO64_SIZE)
        fcntl.ioctl(self.fd, LOOP_GET_STATUS64, raw, True)
        dev, inode, _, offset, limit = struct.unpack_from("=QQQQQ", raw)
        assert (dev, inode, offset, limit) == (self.file.st_dev, self.file.st_ino, 0, 0), (
            f"loop {self.path} does not bind the fixture's exact whole backing file"
        )
        return device

    def check(self):
        device = self.binding()
        path = self.path.lstat()
        assert stat.S_ISBLK(path.st_mode), path
        assert (device.st_dev, device.st_ino, device.st_rdev) == (path.st_dev, path.st_ino, path.st_rdev), path

    def digest(self, size):
        self.check()
        digest = hashlib.sha256()
        offset = 0
        while offset < size:
            chunk = os.pread(self.fd, min(1024 * 1024, size - offset), offset)
            if not chunk:
                raise AssertionError(f"loop fixture shrank at offset {offset}")
            digest.update(chunk)
            offset += len(chunk)
        assert os.pread(self.fd, 1, size) == b"", "whole loop size disagrees"
        return digest.hexdigest()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        try:
            device = self.binding().st_rdev
            os.fsync(self.fd)
            fcntl.ioctl(self.fd, LOOP_CLR_FD, 0)
        finally:
            os.close(self.fd)
        marker = Path(f"/sys/dev/block/{os.major(device)}:{os.minor(device)}/loop/backing_file")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                if marker.read_text().strip() != str(self.backing):
                    return
            except FileNotFoundError:
                return
            time.sleep(0.025)
        raise AssertionError(f"fixture did not detach its exact backing file on {self.path}")


@contextmanager
def mounted(loop, where):
    loop.check()
    where.mkdir(mode=0o700)
    checked(command(["mount", "-t", "ext4", "-o", "ro,noload,nosuid,nodev,noexec",
                     "--", str(loop.path), str(where)], timeout=20))
    try:
        device = loop.binding().st_rdev
        identity = f"{os.major(device)}:{os.minor(device)}"
        with Path("/proc/self/mountinfo").open() as mounts:
            assert any(line.split()[2:5] == [identity, "/", str(where)] for line in mounts), (
                "fixture did not mount its own whole loop"
            )
        yield
    finally:
        checked(command(["umount", "--", str(where)], timeout=20))


def state_dir(work, name):
    directory = work / name
    directory.mkdir(mode=0o700)
    return directory


def stored(directory):
    return (directory / "operation.json").read_bytes()


def trace_fd_calls(trace, device, names):
    wanted = "|".join(names)
    path = re.escape(str(device))
    regex = re.compile(rf"^(?:{wanted})\(\d+<{path}(?:<|>)")
    return [line for line in trace.read_text().splitlines() if regex.match(line)]


class Installer:
    def __init__(self, script, source, receipt, receipt_sha):
        self.tool = [sys.executable, "-B", str(script)]
        self.source = source
        self.receipt = receipt
        self.receipt_sha = receipt_sha
        self.source_sha = json.loads(receipt.read_bytes())["initial_image_sha256"]
        self.size = source.stat().st_size
        assert re.fullmatch(r"[0-9a-f]{64}", self.source_sha), self.source_sha
        assert self.source_sha == sha(source), "fixture receipt does not match the complete source image"

    def run(self, directory, action, loop, *args, trace=None, inject=None):
        loop.check()
        if action != "plan":
            layout = json.loads(stored(directory))["plan"]
            assert layout["target"]["path"] == str(loop.path), "fixture will only reopen its own loop"
        invocation = [*self.tool, action, "--state-dir", str(directory), *map(str, args)]
        if trace is not None:
            strace = ["strace", "-qq", "-yy", "-s", "0", "-e", f"trace={MANAGED_SYSCALLS}",
                      "-o", str(trace)]
            if inject:
                strace += ["-e", "inject=" + inject]
            invocation = [*strace, "--", *invocation]
        return command(invocation)

    def plan(self, directory, loop, *, denial=None, receipt_sha=None):
        result = self.run(directory, "plan", loop, "--source-image", self.source,
                          "--receipt", self.receipt, "--receipt-sha256",
                          receipt_sha or self.receipt_sha, "--target", loop.path, "--allow-loop")
        if denial is not None:
            deny(result, denial)
            assert not (directory / "operation.json").exists(), result
            return None
        receipt = json.loads(checked(result))
        assert receipt["status"] == "Planned", receipt
        assert re.fullmatch(r"[0-9a-f]{64}", receipt["plan_digest"]), receipt
        assert receipt["plan"]["target"]["path"] == str(loop.path), receipt
        assert json.loads(stored(directory)) == receipt, receipt
        return receipt

    def apply(self, directory, loop, planned, **options):
        return self.run(directory, "apply", loop, "--plan-digest", planned["plan_digest"], **options)

    def assert_verified(self, loop, receipt):
        assert receipt["status"] == "Verified", receipt
        assert loop.digest(self.size) == self.source_sha, "installed device differs from the raw source"


def deny(result, reason):
    assert result.returncode != 0, result
    diagnostic = result.stdout + result.stderr
    assert re.search(reason, diagnostic, re.I), diagnostic[-2000:]


def no_target_writes(trace, loop):
    reported = trace_fd_calls(trace, loop.path, ("pwrite64", "pwritev", "pwritev2", "write"))
    assert not reported, reported[:4]


def rejected_media(work, fixture):
    nonzero = sparse(work, "nonzero.raw", fixture.size)
    offset = fixture.size // 3
    with nonzero.open("r+b", buffering=0) as backing:
        backing.seek(offset)
        backing.write(b"PSIOS: preserve data outside signatures")
        os.fsync(backing.fileno())
    before = sha(nonzero)
    with OwnedLoop(nonzero) as disk:
        fixture.plan(state_dir(work, "dirty"), disk, denial=r"non.?zero|not blank|all.zero")
        assert disk.digest(fixture.size) == before

    short = sparse(work, "short.raw", fixture.size - 512)
    with OwnedLoop(short) as disk:
        fixture.plan(state_dir(work, "short"), disk, denial=r"geometry|size|capacity")
        assert os.pread(disk.fd, 1024, 0) == bytes(1024)
    wrong_sector = sparse(work, "sector.raw", fixture.size)
    with OwnedLoop(wrong_sector, sector_size=4096) as disk:
        fixture.plan(state_dir(work, "sector"), disk, denial=r"geometry|sector|512")
        assert os.pread(disk.fd, 1024, 0) == bytes(1024)

    filesystem = sparse(work, "mounted.raw", fixture.size)
    checked(command(["mkfs.ext4", "-q", "-F", str(filesystem)]))
    before = sha(filesystem)
    with OwnedLoop(filesystem) as disk:
        with mounted(disk, work / "mount"):
            fixture.plan(state_dir(work, "mounted"), disk, denial=r"mount|busy|exclusive|holder")
        assert disk.digest(fixture.size) == before

    blank = sparse(work, "held.raw", fixture.size)
    before = sha(blank)
    with OwnedLoop(blank) as disk:
        directory = state_dir(work, "held")
        plan = fixture.plan(directory, disk)
        original_state = stored(directory)
        fd = os.open(disk.path, os.O_RDONLY | os.O_EXCL | os.O_CLOEXEC | os.O_NOFOLLOW)
        try:
            deny(fixture.apply(directory, disk, plan), r"mount|busy|exclusive|holder")
        finally:
            os.close(fd)
        assert stored(directory) == original_state
        assert disk.digest(fixture.size) == before


def rejected_state_storage(work, fixture):
    def inspect(directory):
        return command([*fixture.tool, "inspect", "--state-dir", str(directory)])

    parent = state_dir(work, "unsafe-parent")
    nested = state_dir(parent, "nested-state")
    parent.chmod(0o777)
    try:
        deny(inspect(nested), r"protected root-owned parent")
    finally:
        parent.chmod(0o700)

    volatile = state_dir(work, "volatile-state")
    checked(command(["mount", "-t", "tmpfs", "-o", "size=4m,mode=0700", "tmpfs", str(volatile)]))
    try:
        deny(inspect(volatile), r"persistent local filesystem")
    finally:
        checked(command(["umount", "--", str(volatile)]))


def replacement(work, fixture):
    backing = sparse(work, "previous.raw", fixture.size)
    directory = state_dir(work, "replacement")
    before = sha(backing)
    with OwnedLoop(backing) as disk:
        plan = fixture.plan(directory, disk)
        path = disk.path
    plan_data = stored(directory)
    for replacement_file in (backing, sparse(work, "replacement.raw", fixture.size)):
        with OwnedLoop(replacement_file, reuse=path) as disk:
            seq = Path(f"/sys/class/block/{path.name}/diskseq").read_text().strip()
            assert int(seq) != int(plan["plan"]["target"]["disk_sequence"]), (seq, plan)
            deny(fixture.apply(directory, disk, plan), r"identity|diskseq|replacement|changed|stale|fenc")
            assert stored(directory) == plan_data
            assert disk.digest(fixture.size) == before


def successful_apply(work, fixture):
    backing = sparse(work, "installed.raw", fixture.size)
    directory = state_dir(work, "installed")
    blank = sha(backing)
    with OwnedLoop(backing) as disk:
        opt_in = state_dir(work, "without-loop-opt-in")
        deny(fixture.run(opt_in, "plan", disk, "--source-image", fixture.source,
                         "--receipt", fixture.receipt, "--receipt-sha256", fixture.receipt_sha,
                         "--target", disk.path), r"loop.*(explicit|opt.?in)")
        assert not (opt_in / "operation.json").exists()
        fixture.plan(state_dir(work, "wrong-receipt"), disk, denial=r"(SHA-256|receipt).*mismatch|digest",
                     receipt_sha="0" * 64)
        plan = fixture.plan(directory, disk)
        plan_data = stored(directory)
        deny(fixture.run(directory, "apply", disk, "--plan-digest", "0" * 64),
             r"plan|approval|digest")
        assert stored(directory) == plan_data
        assert disk.digest(fixture.size) == blank

        effects = work / "write-trace.txt"
        applied = json.loads(checked(fixture.apply(directory, disk, plan, trace=effects)))
        assert applied["plan_digest"] == plan["plan_digest"], applied
        assert json.loads(stored(directory)) == applied
        fixture.assert_verified(disk, applied)
        assert trace_fd_calls(effects, disk.path, ("pwrite64",)), "missing real target writes"
        flushes = trace_fd_calls(effects, disk.path, ("fsync",))
        assert len(flushes) == 2 and all(line.endswith("= 0") for line in flushes), flushes

        replay = work / "verified-replay-trace.txt"
        durable = stored(directory)
        assert json.loads(checked(fixture.apply(directory, disk, plan, trace=replay))) == applied
        no_target_writes(replay, disk)
        assert stored(directory) == durable
        assert json.loads(checked(fixture.run(directory, "inspect", disk))) == applied

        damaged = state_dir(work, "damaged-receipt")
        record = damaged / "operation.json"
        record.write_text(json.dumps({**applied, "image_readback_sha256": "0" * 64}))
        record.chmod(0o600)
        deny(fixture.run(damaged, "inspect", disk), r"verified receipt")
    return backing, effects, disk.path


def partial_write(work, fixture):
    backing = sparse(work, "partial.raw", fixture.size)
    directory = state_dir(work, "partial")
    before = sha(backing)
    with OwnedLoop(backing) as disk:
        plan = fixture.plan(directory, disk)
        trace = work / "partial-trace.txt"
        stopped = fixture.apply(directory, disk, plan, trace=trace,
                                inject="pwrite64:error=EINTR:signal=SIGKILL:when=2")
        assert stopped.returncode in (-signal.SIGKILL, 128 + signal.SIGKILL), stopped
        writes = trace_fd_calls(trace, disk.path, ("pwrite64",))
        assert len(writes) == 2 and re.search(r"=\s+[1-9][0-9]*$", writes[0]), writes
        assert writes[1].endswith("= ?") or "INJECTED" in writes[1], writes
        status = json.loads(stored(directory))
        assert (status["status"], status["plan_digest"]) == ("WriteIssued", plan["plan_digest"]), status
        partial = disk.digest(fixture.size)
        assert partial not in (before, fixture.source_sha), "fault did not leave a genuinely partial write"
        partitions = plan["plan"]["source"]["partitions"]
        first = min(part["start_sector"] for part in partitions) * 512
        last = max(part["start_sector"] + part["sector_count"] for part in partitions) * 512
        assert os.pread(disk.fd, first, 0) == bytes(first), "primary GPT was published before payload flush"
        assert os.pread(disk.fd, fixture.size - last, last) == bytes(fixture.size - last), (
            "backup GPT was published before payload flush"
        )

        issued = stored(directory)
        for action in ("apply", "inspect"):
            replay = work / f"partial-{action}-trace.txt"
            result = (fixture.apply(directory, disk, plan, trace=replay) if action == "apply"
                      else fixture.run(directory, action, disk, trace=replay))
            deny(result, r"recover|issu|partial|matches|digest|interrupted")
            if action == "inspect":
                assert json.loads(result.stdout)["status"] == "RecoveryRequired", result
            no_target_writes(replay, disk)
            assert stored(directory) == issued
            assert disk.digest(fixture.size) == partial


def completed_before_receipt(work, fixture, successful_trace, successful_device):
    all_fsyncs = [line for line in successful_trace.read_text().splitlines() if line.startswith("fsync(")]
    _, target_fsync = trace_fd_calls(successful_trace, successful_device, ("fsync",))
    next_fsync = max(index for index, line in enumerate(all_fsyncs) if line == target_fsync) + 2
    assert next_fsync <= len(all_fsyncs), all_fsyncs
    # Kill on the first receipt-journal fsync after the real target fsync.
    backing = sparse(work, "completed-before-receipt.raw", fixture.size)
    directory = state_dir(work, "completed-before-receipt")
    with OwnedLoop(backing) as disk:
        plan = fixture.plan(directory, disk)
        trace = work / "completed-before-receipt-trace.txt"
        stopped = fixture.apply(directory, disk, plan, trace=trace,
                                inject=f"fsync:error=EINTR:signal=SIGKILL:when={next_fsync}")
        assert stopped.returncode in (-signal.SIGKILL, 128 + signal.SIGKILL), stopped
        flushes = trace_fd_calls(trace, disk.path, ("fsync",))
        assert len(flushes) == 2 and all(line.endswith("= 0") for line in flushes), flushes
        issued = json.loads(stored(directory))
        assert (issued["status"], issued["plan_digest"]) == ("WriteIssued", plan["plan_digest"]), issued
        assert disk.digest(fixture.size) == fixture.source_sha
        deny(fixture.apply(directory, disk, plan), r"already issued|inspect")
        inspection = work / "reconciled-trace.txt"
        recovered = json.loads(checked(fixture.run(directory, "inspect", disk, trace=inspection)))
        no_target_writes(inspection, disk)
        fixture.assert_verified(disk, recovered)
        assert recovered["reconciled"] is True and recovered["plan_digest"] == plan["plan_digest"], recovered
        assert recovered["image_readback_sha256"] == fixture.source_sha, recovered
        assert json.loads(stored(directory)) == recovered
    backing.unlink()


def source_fixture(args, work):
    if args.source_image:
        source = args.source_image.resolve(strict=True)
        receipt = args.receipt.resolve(strict=True)
        secret = args.console_password_file.read_text().rstrip("\r\n") if args.console_password_file else None
        return source, receipt, args.receipt_sha256, secret
    secret, console_hash = password(work, "operator.sha512crypt")
    source = work / "fresh-source.raw"
    receipt = work / "build-receipt.json"
    archive = args.archive.resolve(strict=True)
    result = command([sys.executable, "-B", str(args.builder.resolve(strict=True)), str(source),
                      "--kernel-iso", str(args.kernel_iso.resolve(strict=True)), "--core-archive", str(archive),
                      "--core-sha256", sha(archive), "--console-hash", str(console_hash)], timeout=330)
    receipt.write_text(checked(result))
    return source, receipt, sha(receipt), secret


def check_factory_staging(work, installer, source, receipt, approval):
    # Exercise the source validator shipped alongside the installer under test.
    sys.path.insert(0, str(installer.resolve(strict=True).parent))
    from install_source import stage_image

    with tempfile.TemporaryDirectory(dir=work) as staging:
        image, facts = stage_image(Path(staging), source, receipt, approval)
        copied = image.stat()
        assert copied.st_size == source.stat().st_size == facts["size_bytes"]
        assert sha(image) == facts["image_sha256"] == sha(source)
        # The freshly formatted state and home partitions have ample empty space.
        assert copied.st_blocks * 512 < copied.st_size // 2, (copied.st_size, copied.st_blocks)


def exercise(args, work):
    source, receipt, approval, secret = source_fixture(args, work)
    if not args.source_image:
        check_factory_staging(work, args.installer, source, receipt, approval)
    fixture = Installer(args.installer.resolve(strict=True), source, receipt, approval)
    rejected_state_storage(work, fixture)
    rejected_media(work, fixture)
    replacement(work, fixture)
    installed, trace, device = successful_apply(work, fixture)
    completed_before_receipt(work, fixture, trace, device)
    partial_write(work, fixture)

    if not args.no_boot:
        for source_name, target_name in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
            shutil.copyfile(args.ovmf_dir / source_name, work / target_name)
        with boot(work, installed) as console:
            console.login("psios", secret)
            observed = check_state(console)
            assert observed[2]["availability"] == "Available", "installed Core did not persist its result"
            deployment = console.run("sha256sum /psios-deployment.json").split()[0]
            assert deployment == json.loads(receipt.read_bytes())["deployment_manifest_sha256"]
    assert sha(fixture.source) == fixture.source_sha
    print("PASS native blank installation on disposable whole loop devices"
          + (" and OVMF boot of the installed disk" if not args.no_boot else ""))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/install.py")
    parser.add_argument("--builder", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--kernel-iso", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--source-image", type=Path, help="prebuilt fresh raw native disk")
    parser.add_argument("--receipt", type=Path, help="trusted build receipt for the prebuilt disk")
    parser.add_argument("--receipt-sha256", help="trusted complete receipt digest for the prebuilt disk")
    parser.add_argument("--console-password-file", type=Path, help="fixture psios credential for prebuilt disk boot")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--no-boot", action="store_true", help="exercise loop media only")
    args = parser.parse_args()
    if args.source_image:
        if not args.receipt or not re.fullmatch(r"[0-9a-f]{64}", args.receipt_sha256 or ""):
            parser.error("prebuilt source requires --receipt and the exact --receipt-sha256")
        if not args.no_boot and args.console_password_file is None:
            parser.error("booting a prebuilt source requires --console-password-file")
        if args.kernel_iso or args.archive:
            parser.error("choose either the prebuilt source or --kernel-iso and --archive")
    elif not args.kernel_iso or not args.archive or args.receipt or args.receipt_sha256 or args.console_password_file:
        parser.error("building a fresh fixture requires --kernel-iso and --archive")
    if os.geteuid() != 0:
        parser.error("the disposable loop and private mount fixture requires root")
    tools = ["losetup", "mkfs.ext4", "mount", "umount", "unshare", "strace"]
    if not args.no_boot:
        tools.append("qemu-system-x86_64")
    missing = [tool for tool in tools if shutil.which(tool) is None]
    if missing:
        parser.error("fixture requires " + ", ".join(missing))
    current_ns = os.readlink("/proc/self/ns/mnt")
    outer_ns = os.getenv(OUTER_MOUNT_NS)
    if outer_ns is None:
        program = [sys.executable, "-B", str(Path(__file__).resolve()), *sys.argv[1:]]
        return subprocess.run(["unshare", "--mount", "--propagation", "private", "--", *program],
                              env=dict(os.environ, **{OUTER_MOUNT_NS: current_ns}), check=False).returncode
    if current_ns == outer_ns:
        parser.error("the installer fixture did not enter a private mount namespace")
    with tempfile.TemporaryDirectory(prefix="psios-blank-install-test-") as temporary:
        exercise(args, Path(temporary).resolve())
    return 0


if __name__ == "__main__":
    sys.exit(main())
