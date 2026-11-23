"""Boot a generated disk from OVMF, use its local console and recover after power loss."""
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import secrets
import select
import shutil
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]


def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


class Console:
    def __init__(self, vm):
        self.vm = vm
        self.buf = b""

    def until(self, marker, timeout=75):
        end = time.monotonic() + timeout
        while marker not in self.buf:
            remaining = end - time.monotonic()
            if remaining <= 0 or not select.select([self.vm.stdout], [], [], remaining)[0]:
                raise AssertionError(f"guest did not reach {marker!r}")
            chunk = os.read(self.vm.stdout.fileno(), 8192)
            if not chunk:
                raise AssertionError(f"guest stopped before {marker!r}")
            self.buf += chunk.replace(b"\r", b"")
            if len(self.buf) > 1024 * 1024:
                raise AssertionError("guest serial output exceeded the limit")
        data, _, self.buf = self.buf.partition(marker)
        return data

    def send(self, line):
        self.vm.stdin.write(line.encode() + b"\r")
        self.vm.stdin.flush()

    def credentials(self, user, password):
        self.until(b"login:")
        self.send(user)
        self.until(b"Password:")
        self.send(password)

    def login(self, user, password):
        self.credentials(user, password)
        self.until(b"# " if user == "root" else b"$ ")
        marker = "PSIOSLOGIN" + secrets.token_hex(6)
        self.send(f"stty -echo; export PS1=''; printf '\\n{marker}\\n'")
        self.until(("\n" + marker + "\n").encode())

    def run(self, command):
        marker = "PSIOSCMD" + secrets.token_hex(6)
        self.send(f"printf '\\n{marker}\\n'; ( {command} ); r=$?; printf '\\n{marker}:%s\\n' \"$r\"")
        self.until(("\n" + marker + "\n").encode())
        output = self.until(("\n" + marker + ":").encode())
        if self.until(b"\n") != b"0":
            raise AssertionError(f"guest command failed: {command}")
        return output.decode().strip()


@contextmanager
def boot(work, image):
    accel = "kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    cmd = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-machine", "q35",
           "-accel", accel, "-m", "768", "-smp", "2", "-display", "none", "-nic", "none",
           "-monitor", "none", "-serial", "stdio", "-boot", "order=c,strict=on",
           "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=code.fd",
           "-drive", "if=pflash,format=raw,unit=1,file=vars.fd",
           "-drive", "if=virtio,format=raw,file=" + image.name]
    with subprocess.Popen(cmd, cwd=work, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as vm:
        try:
            yield Console(vm)
        finally:
            if vm.poll() is None:
                vm.kill()
            vm.wait(15)


def password(work, name):
    secret = secrets.token_urlsafe(26)
    path = work / name
    with path.open("xb") as f:
        os.fchmod(f.fileno(), 0o600)
        subprocess.run(["openssl", "passwd", "-6", "-stdin"], input=(secret + "\n").encode(), stdout=f, check=True)
    return secret, path


def check_state(c):
    assert c.run("id -u") == "1000"
    assert c.run(". /etc/os-release; echo \"$ID\"") == "psios"
    assert c.run("awk '/^Uid:/ {print $2}' /proc/$(pidof psiosd)/status") == "1000"
    c.run("test -d /sys/firmware/efi && test -d /sys/class/net/lo || exit 1; "
          "for net in /sys/class/net/*; do test ! -e \"$net/device\" || exit 1; done")
    who = json.loads(c.run("psiosctl whoami"))
    assert who["principal_id"] == "local-owner"
    submitted = json.loads(c.run("printf '{}' | psiosctl submit system.status --key native-power-cut"))
    result = json.loads(c.run("psiosctl wait " + submitted["task_id"]))
    assert result["lifecycle"] == "Succeeded"
    return who["authority_id"], submitted, result["result"], c.run("cat /proc/sys/kernel/random/boot_id")


def replace_partition(image, filesystem, offset_mib):
    with image.open("r+b") as dst, filesystem.open("rb") as src:
        dst.seek(offset_mib * 1024 * 1024)
        shutil.copyfileobj(src, dst)


def extract_partition(image, output, offset_mib, size_mib):
    with image.open("rb") as src, output.open("wb") as dst:
        src.seek(offset_mib * 1024 * 1024)
        for _ in range(size_mib):
            chunk = src.read(1024 * 1024)
            assert len(chunk) == 1024 * 1024
            dst.write(chunk)


def exercise(args, work):
    operator, op_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    image = work / "installed.raw"
    build = [sys.executable, args.installer.resolve(), image, "--kernel-iso", args.kernel_iso,
             "--core-archive", args.archive, "--core-sha256", sha(args.archive), "--console-hash", op_hash,
             "--root-console-hash", root_hash]
    deployment = json.loads(subprocess.check_output(build))
    before = sha(image)
    assert subprocess.run(build, capture_output=True).returncode != 0 and sha(image) == before
    assert image.stat().st_mode & 0o077 == 0
    for source, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / source, work / target)
    with boot(work, image) as c:
        c.credentials("psios", "wrong-" + secrets.token_hex(10))
        c.until(b"Login incorrect")
        c.login("psios", operator)
        assert c.run("pwd") == "/home/psios"
        assert c.run("stat -c '%u:%g:%a' .") == "1000:1000:700"
        c.run("test ! -r /root && test ! -w /mnt/psios-homes")
        c.run("printf '#!/bin/sh\\necho persisted\\n' > ./observe && chmod 700 ./observe && sync -f .")
        assert c.run("./observe") == "persisted"
        # Fill only the user's filesystem; a fresh Core submission still needs
        # durable writes to the authority and its witness on partition two.
        failure = c.run("dd if=/dev/zero of=./full bs=64K 2>&1; test $? -ne 0")
        assert "No space left on device" in failure, failure
        assert int(c.run("stat -c %s ./full")) >= 40 * 1024 * 1024
        first = check_state(c)
        assert not first[1]["deduplicated"]
        # SIGKILL the VM with the OS and Core still running.
    with boot(work, image) as c:
        c.login("psios", operator)
        assert c.run("./observe") == "persisted"
        c.run("rm ./full && sync -f .")
        second = check_state(c)
        assert first[0] == second[0] and second[1]["deduplicated"]
        assert first[1]["task_id"] == second[1]["task_id"] and first[2] == second[2] and first[3] != second[3]
        c.send("exit")
        c.login("root", admin)
        assert c.run("id -u") == "0"
        assert c.run("pwd; stat -c '%u:%g:%a' .").splitlines() == ["/root", "0:0:700"]
        c.run("umask 077; echo operator-rotated > ./maintenance")
        # passwd replaces /etc/shadow atomically. The host account database must
        # survive a reboot without restoring the credentials from initial enrollment.
        rotated = secrets.token_urlsafe(26)
        c.send("passwd psios")
        c.until(b"New password:")
        c.send(rotated)
        c.until(b"Retype password:")
        c.send(rotated)
        c.until(b"password for psios changed")
        c.send("poweroff")
        assert c.vm.wait(35) == 0
    with boot(work, image) as c:
        c.credentials("psios", operator)
        c.until(b"Login incorrect")
        c.login("psios", rotated)
        assert c.run("./observe") == "persisted"
        c.run("test ! -r /root/maintenance")
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == first[0]
        c.send("exit")
        c.login("root", admin)
        assert c.run("cat ./maintenance") == "operator-rotated"
        c.send("poweroff")
        assert c.vm.wait(35) == 0
    # Inspect the *installed* second partition from the external read-only rescue kernel.
    report = subprocess.check_output([sys.executable, args.inspector, image, "--iso", args.kernel_iso,
                                      "--core", args.psiosd, "--core-sha256", sha(args.psiosd),
                                      "--partition", "2", "--state", "/psios", "--uid", "1000"])
    checked = json.loads(report)
    assert checked["sqlite_integrity"] == "Verified" and checked["node_id"] == first[0] and checked["foreign_keys"] == "Verified"
    # A valid, independently formatted home volume cannot replace the expected
    # filesystem; detecting that mismatch must precede writing either volume.
    original_home = work / "original-home.ext4"
    extract_partition(image, original_home, 257, 64)
    foreign_home = work / "foreign-home.ext4"
    with foreign_home.open("xb") as f:
        f.truncate(64 * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", foreign_home], check=True)
    replace_partition(image, foreign_home, 257)
    mismatched = sha(image)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at home-device")
        assert c.vm.wait(25) == 0
    assert sha(image) == mismatched
    replace_partition(image, original_home, 257)
    # Lose only the database of the stopped guest. The consumed enrollment must
    # never authorize a second initialization, even with the genuine disk UUID.
    partition = work / "state.ext4"
    extract_partition(image, partition, 65, 192)
    def read_owner():
        return subprocess.check_output(["debugfs", "-R", "cat /psios.witness/owner.token", partition],
                                       stderr=subprocess.DEVNULL)
    original_owner = read_owner()
    assert len(original_owner) == 64
    subprocess.run(["debugfs", "-w", "-R", "rm /psios/state.db", partition], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    replace_partition(image, partition, 65)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at authority")
        assert c.vm.wait(25) == 0
    extract_partition(image, partition, 65, 192)
    assert read_owner() == original_owner
    absent = subprocess.run(["debugfs", "-R", "stat /psios/state.db", partition], capture_output=True)
    assert b"File not found" in absent.stderr
    partition.unlink()
    # Swap in a different ext4 filesystem on a disposable copy, including a fresh enrollment marker.
    alien = work / "other.ext4"
    seed = work / "foreign"
    seed.mkdir()
    (seed / "initialize-once").touch()
    with alien.open("xb") as f:
        f.truncate(192 * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", "-d", seed, alien], check=True)
    replace_partition(image, alien, 65)
    pre_reject = sha(image)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at state-device")
        assert c.vm.wait(25) == 0
    assert sha(image) == pre_reject
    print(f"Verified source {deployment['source_commit']}, node {first[0]}, task {first[1]['task_id']}, "
          "authenticated UEFI console, private persistent homes, full-home isolation, offline management, power loss, clean shutdown, external rescue, consumed enrollment and mismatched state/home refusal.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--kernel-iso", "--archive", "--psiosd", "--inspector"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    opts = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-installed-test-") as directory:
        exercise(opts, Path(directory))
