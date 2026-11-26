"""Boot one managed native disk and verify its normal Core console."""

from contextlib import ExitStack, contextmanager
import ctypes
import json
import math
import os
from pathlib import Path
import re
import secrets
import select
import shutil
import signal
import stat
import subprocess
import time
import uuid


MIB = 1024 * 1024
MAX_DISK_BYTES = 8192 * MIB
SERIAL_LIMIT = 128 * 1024
BOOT_SECONDS = 115
CORE_SECONDS = 40
SHUTDOWN_SECONDS = 60
PR_SET_PDEATHSIG = 1
PROTECTED_OPEN = os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
AS_OWNER = "/bin/busybox su -s /bin/sh psios -c"
EFI_GLOBAL_VARIABLE = "8be4df61-93ca-11d2-aa0d-00e098032b8c"


def require(condition, message):
    if not condition:
        raise ValueError(message)


@contextmanager
def open_directory(work):
    owned_fds = Path(f"/proc/{os.getpid()}/fd")
    if work.parent == owned_fds and re.fullmatch(r"0|[1-9][0-9]{0,9}", work.name):
        fd = os.dup(int(work.name))
    else:
        fd = os.open(work, PROTECTED_OPEN | os.O_RDONLY | os.O_DIRECTORY)
    try:
        info = os.fstat(fd)
        require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid()
                and stat.S_IMODE(info.st_mode) == 0o700,
                "candidate files require a private operator-owned directory")
        yield fd
    finally:
        os.close(fd)


@contextmanager
def open_file(directory, name, *, writable):
    mode = os.O_RDWR if writable else os.O_RDONLY
    fd = os.open(name, PROTECTED_OPEN | mode, dir_fd=directory)
    try:
        info = os.fstat(fd)
        allowed_modes = (0o600,) if writable else (0o400, 0o600)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid() and info.st_nlink == 1
                and stat.S_IMODE(info.st_mode) in allowed_modes and info.st_size > 0,
                "candidate file must be a private, nonempty regular file")
        yield fd, info.st_size
    finally:
        os.close(fd)


def parent_death_guard():
    parent = os.getpid()
    prctl = ctypes.CDLL(None, use_errno=True).prctl
    prctl.argtypes = (ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong)
    prctl.restype = ctypes.c_int

    def arm():
        # This runs in the single-threaded controller's child before exec.
        # Check the parent again after arming to close the fork-to-prctl race.
        if prctl(PR_SET_PDEATHSIG, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent:
            os._exit(126)

    return arm


def wait_fd(fd, events, deadline):
    left = deadline - time.monotonic()
    require(left > 0, "candidate serial console exceeded its deadline")
    poller = select.poll()
    poller.register(fd, events)
    require(poller.poll(math.ceil(left * 1000)), "candidate serial console exceeded its deadline")


class SerialConsole:
    def __init__(self, process):
        self.read_fd = process.stdout.fileno()
        self.write_fd = process.stdin.fileno()
        os.set_blocking(self.read_fd, False)
        os.set_blocking(self.write_fd, False)
        self.received = bytearray()
        self.total = 0

    def receive(self, deadline):
        while True:
            wait_fd(self.read_fd, select.POLLIN, deadline)
            try:
                chunk = os.read(self.read_fd, 8192)
                break
            except BlockingIOError:
                continue
        self.total += len(chunk)
        require(self.total <= SERIAL_LIMIT, "candidate serial output exceeds its limit")
        self.received.extend(chunk.replace(b"\r", b""))
        return bool(chunk)

    def until(self, marker, deadline, *, reject=None):
        while True:
            position = self.received.find(marker)
            if reject is not None:
                rejected = self.received.find(reject)
                require(rejected < 0 or (position >= 0 and position < rejected),
                        "candidate root console credentials were rejected")
            if position >= 0:
                before = bytes(self.received[:position])
                del self.received[:position + len(marker)]
                return before
            require(self.receive(deadline), "candidate stopped before completing its console proof")

    def write(self, data, deadline):
        pending = memoryview(data)
        while pending:
            wait_fd(self.write_fd, select.POLLOUT, deadline)
            try:
                pending = pending[os.write(self.write_fd, pending):]
            except BlockingIOError:
                continue

    def send_line(self, line, deadline):
        self.write(line.encode("ascii") + b"\r", deadline)

    def login(self, credential):
        deadline = time.monotonic() + BOOT_SECONDS
        self.until(b"login: ", deadline)
        self.send_line("root", deadline)
        self.until(b"Password: ", deadline)
        # Allow BusyBox to turn off echo and pace password bytes for the UART.
        time.sleep(0.03)
        for start in range(0, len(credential), 8):
            self.write(credential[start:start + 8], deadline)
            time.sleep(0.01)
        self.write(b"\r", deadline)
        prefix = self.until(b"# ", deadline, reject=b"Login incorrect")
        require(credential not in prefix.splitlines(), "candidate console echoed its credential")

    def command(self, command, deadline):
        nonce = "PSIOSACT" + secrets.token_hex(12)
        self.send_line(f"printf '\\n{nonce}\\n'; ( {command} ); r=$?; printf '\\n{nonce}:%s\\n' \"$r\"", deadline)
        self.until(f"\n{nonce}\n".encode("ascii"), deadline)
        result = self.until(f"\n{nonce}:".encode("ascii"), deadline)
        require(self.until(b"\n", deadline) == b"0", "candidate console proof command failed")
        return result.strip()

    def shutdown(self, process):
        deadline = time.monotonic() + SHUTDOWN_SECONDS
        self.send_line("cd / && exec /bin/busybox poweroff", deadline)
        while self.receive(deadline):
            pass
        require(b"Power down" in self.received and b"PsiOS shutdown could not close" not in self.received,
                "candidate did not report a clean shutdown")
        require(process.wait(timeout=max(0, deadline - time.monotonic())) == 0,
                "candidate QEMU failed during shutdown")


def unique_fields(pairs):
    fields = dict(pairs)
    require(len(fields) == len(pairs), "duplicate candidate Core response fields")
    return fields


def core_object(body):
    try:
        value = json.loads(body.decode("utf-8"), object_pairs_hook=unique_fields)
    except (ValueError, RecursionError):
        raise ValueError("candidate Core returned an invalid identity response") from None
    require(isinstance(value, dict), "candidate Core identity response is not an object")
    return value


def verify_secure_boot(console, deadline):
    console.command("test -f /psios-secure-boot-required && test ! -L /psios-secure-boot-required", deadline)
    mode = console.command(
        "if ! /bin/busybox grep -qw efivarfs /proc/filesystems; then "
        "/bin/busybox modprobe efivarfs >/dev/null 2>&1 || :; fi; "
        "if /bin/busybox grep -qw efivarfs /proc/filesystems; then printf 'efivarfs'; else printf 'kernel'; fi",
        deadline,
    )
    if mode == b"kernel":
        console.command(
            "/bin/busybox dmesg | /bin/busybox grep -Eiq 'secure boot enabled|locked down from EFI Secure Boot'",
            deadline,
        )
        return
    require(mode == b"efivarfs", "candidate did not report its Secure Boot evidence source")
    # efivarfs prepends four attribute bytes to each EFI variable value.
    values = console.command(
        "path=/sys/firmware/efi/efivars; mounted=0; "
        "if ! /bin/busybox grep -Fq ' /sys/firmware/efi/efivars efivarfs ' /proc/mounts; then "
        "/bin/busybox mount -t efivarfs -o ro,nosuid,nodev,noexec efivarfs \"$path\" || exit 1; mounted=1; fi; "
        f"secure=$(/bin/busybox od -An -j 4 -N 1 -t u1 \"$path/SecureBoot-{EFI_GLOBAL_VARIABLE}\") || exit 1; "
        f"setup=$(/bin/busybox od -An -j 4 -N 1 -t u1 \"$path/SetupMode-{EFI_GLOBAL_VARIABLE}\") || exit 1; "
        "if [ \"$mounted\" = 1 ]; then /bin/busybox umount \"$path\" || exit 1; fi; "
        "printf '%s %s' \"$secure\" \"$setup\"",
        deadline,
    )
    require(values.split() == [b"1", b"0"], "candidate firmware did not enforce Secure Boot")


def console_proof(console, credential, expected_node, manifest_sha256, *, secure_boot_required=False):
    console.login(credential)
    deadline = time.monotonic() + CORE_SECONDS
    console.command("test \"$(/bin/busybox id -u)\" = 0 && test -d /sys/firmware/efi && "
                    "/bin/busybox stty -echo", deadline)
    whoami = f"{AS_OWNER} 'exec /bin/psiosctl whoami'"
    console.command(f"for i in $(/bin/busybox seq 1 120); do "
                    f"{whoami} >/dev/null 2>&1 && exit 0; /bin/busybox sleep 0.2; done; exit 1", deadline)

    try:
        boot_id = console.command("/bin/busybox cat /proc/sys/kernel/random/boot_id", deadline).decode("ascii")
        identity = uuid.UUID(boot_id)
        require(str(identity) == boot_id and identity.int != 0, "noncanonical boot identity")
    except ValueError:
        raise ValueError("candidate reported an invalid Linux boot ID") from None

    manifest = console.command("/bin/busybox sha256sum /psios-deployment.json", deadline)
    matches = re.findall(rb"^([0-9a-f]{64})  /psios-deployment\.json$", manifest, re.MULTILINE)
    require(len(matches) == 1, "candidate did not report one deployment digest")
    require(matches[0].decode("ascii") == manifest_sha256,
            f"candidate booted a different deployment ({matches[0].decode('ascii')})")
    if secure_boot_required:
        verify_secure_boot(console, deadline)
    caller = core_object(console.command(whoami, deadline))
    node = core_object(console.command(f"{AS_OWNER} 'exec /bin/psiosctl node'", deadline))
    reference = node.get("reference")
    require(caller.get("principal_id") == "local-owner" and caller.get("authority_id") == expected_node
            and isinstance(reference, dict) and reference.get("resource_id") == expected_node,
            "candidate Core reported a different original node identity")
    proof = {"boot_id": boot_id, "node_id": expected_node, "deployment_manifest_sha256": manifest_sha256}
    if secure_boot_required:
        proof["secure_boot"] = "Enforced"
    return proof


def qemu_command(disk_fd, code_fd, vars_fd, vars_probe_fd, *, secure_boot_required=False):
    located = shutil.which("qemu-system-x86_64")
    require(located is not None, "qemu-system-x86_64 is required for native activation")
    executable = Path(located).resolve(strict=True)
    info = executable.stat()
    require(stat.S_ISREG(info.st_mode) and not info.st_mode & (stat.S_ISUID | stat.S_ISGID),
            "native activation requires an ordinary QEMU executable")
    machine = ["-machine", "q35"]
    if secure_boot_required:
        machine = ["-machine", "q35,smm=on", "-global", "driver=cfi.pflash01,property=secure,value=on"]
    return [
        executable, "-nodefaults", "-no-user-config", "-no-reboot", "-accel", "tcg,thread=single",
        *machine, "-m", "768", "-smp", "1", "-display", "none", "-monitor", "none",
        "-nic", "none", "-boot", "strict=on", "-serial", "stdio",
        "-add-fd", f"fd={disk_fd},set=1", "-add-fd", f"fd={code_fd},set=2", "-add-fd", f"fd={vars_fd},set=3",
        "-add-fd", f"fd={vars_probe_fd},set=3",
        "-drive", "if=pflash,format=raw,unit=0,file=/dev/fdset/2,readonly=on",
        "-drive", "if=pflash,format=raw,unit=1,file=/dev/fdset/3",
        "-blockdev", "driver=file,node-name=candidate-file,filename=/dev/fdset/1,locking=on",
        "-blockdev", "driver=raw,node-name=candidate,file=candidate-file",
        "-device", "virtio-blk-pci,drive=candidate,bootindex=1,werror=report,rerror=report",
    ]


def boot_candidate(work: Path, *, disk: Path, firmware_code: Path, firmware_vars: Path,
                   credential: bytes, expected_node: str, manifest_sha256: str, before_launch,
                   secure_boot_required: bool = False) -> dict:
    """Confirm a bounded normal OVMF boot and clean shutdown of one owned disk."""
    work = work.absolute()
    paths = [path.absolute() for path in (disk, firmware_code, firmware_vars)]
    require(len(set(paths)) == len(paths) and all(path.parent == work for path in paths)
            and ".." not in work.parts, "candidate files must be distinct direct children of the private directory")
    require(isinstance(credential, bytes) and 0 < len(credential) <= 128
            and all(32 <= byte <= 126 for byte in credential),
            "candidate root console credential must be bounded printable ASCII")
    require(isinstance(expected_node, str) and 0 < len(expected_node) <= 128
            and isinstance(manifest_sha256, str) and re.fullmatch(r"[0-9a-f]{64}", manifest_sha256),
            "candidate boot needs an exact node and deployment digest")
    require(type(secure_boot_required) is bool, "Secure Boot requirement must be a boolean")

    with open_directory(work) as directory, ExitStack() as files:
        disk_fd, disk_size = files.enter_context(open_file(directory, paths[0].name, writable=True))
        code_fd, code_size = files.enter_context(open_file(directory, paths[1].name, writable=False))
        vars_fd, vars_size = files.enter_context(open_file(directory, paths[2].name, writable=True))
        # QEMU first probes writable pflash with an O_RDONLY open of the same store.
        vars_probe_fd, _ = files.enter_context(open_file(directory, paths[2].name, writable=False))
        require(os.path.samestat(os.fstat(vars_fd), os.fstat(vars_probe_fd)),
                "OVMF variable store changed before launch")
        require(disk_size <= MAX_DISK_BYTES and disk_size % 512 == 0, "invalid raw candidate disk size")
        require(code_size + vars_size == 4 * MIB, "expected a matching 4 MiB OVMF code and variable-store pair")
        fds = (disk_fd, code_fd, vars_fd, vars_probe_fd)
        command = qemu_command(*fds, secure_boot_required=secure_boot_required)
        guard = parent_death_guard()
        before_launch()
        process = subprocess.Popen(command, cwd="/", stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, pass_fds=fds, preexec_fn=guard, bufsize=0)
        try:
            console = SerialConsole(process)
            proof = console_proof(console, credential, expected_node, manifest_sha256,
                                  secure_boot_required=secure_boot_required)
            console.shutdown(process)
            os.fsync(disk_fd)
            os.fsync(vars_fd)
            return proof
        except ValueError as error:
            status = process.poll()
            if status is not None and status != 0:
                raise ValueError(f"candidate QEMU exited with status {status}. {error}") from None
            raise
        except (OSError, subprocess.TimeoutExpired):
            raise ValueError("candidate console, shutdown or storage synchronization failed") from None
        finally:
            process.stdin.close()
            process.stdout.close()
            if process.poll() is None:
                process.kill()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    raise ValueError("candidate QEMU did not terminate after kill") from None
