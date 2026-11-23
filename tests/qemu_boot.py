"""Boot two freshly provisioned guests and restart one from its own disk."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
CTL = "sudo -n -u psios /usr/bin/psiosctl --socket /run/psios/api.sock"


@contextlib.contextmanager
def boot(instance, key, number):
    # Bind forwarding only on loopback. restrict=on prevents guest access to
    # external package mirrors, registries or model endpoints during acceptance.
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    accel = "kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    serial = instance / f"serial-{number}.log"
    with subprocess.Popen([
        "qemu-system-x86_64", "-accel", accel, "-cpu", "host" if accel == "kvm" else "max",
        "-m", "1536", "-smp", "2", "-display", "none", "-monitor", "none", "-no-reboot",
        "-serial", "file:" + str(serial), "-drive", "file=system.qcow2,format=qcow2,if=virtio",
        "-drive", "file=seed.iso,format=raw,media=cdrom,readonly=on",
        "-nic", f"user,model=virtio-net-pci,restrict=on,hostfwd=tcp:127.0.0.1:{port}-:22",
    ], cwd=instance) as vm:
        ssh = ["ssh", "-T", "-p", str(port), "-i", str(key), "-o", "BatchMode=yes",
               "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=4",
               "-o", "HostKeyAlias=" + instance.name, "-o", "UserKnownHostsFile=" + str(instance / "known_hosts"),
               "psios-admin@127.0.0.1"]

        def run(command, *, check=True, timeout=40):
            return subprocess.run(ssh + [command], check=check, text=True, capture_output=True, timeout=timeout)
        try:
            deadline = time.monotonic() + 360
            while time.monotonic() < deadline:
                if vm.poll() is not None:
                    raise RuntimeError("guest exited before provisioning")
                status = run("sudo -n test -f /var/lib/psios-provision/complete && systemctl is-active --quiet psiosd && " + CTL + " whoami >/dev/null",
                             check=False)
                if status.returncode == 0:
                    break
                if status.returncode != 255 and run("systemctl is-failed --quiet psios-provision", check=False).returncode == 0:
                    raise RuntimeError(run("sudo -n journalctl -b -u psios-provision -u psiosd --no-pager", check=False).stdout)
                time.sleep(2)
            else:
                raise RuntimeError("guest provisioning readiness timeout")
            yield run
            run("sudo -n systemctl poweroff", check=False)
            if vm.wait(timeout=90) != 0:
                raise RuntimeError("guest did not shut down cleanly")
        except BaseException:
            if serial.exists():
                print(serial.read_bytes()[-5000:].decode(errors="replace"), file=sys.stderr)
            raise
        finally:
            if vm.poll() is None:
                vm.terminate()
                try:
                    vm.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    vm.kill()


def identity(run):
    identity = json.loads(run(CTL + " node").stdout)["reference"]["resource_id"]
    return identity, run("cat /etc/machine-id").stdout.strip(), run("cat /proc/sys/kernel/random/boot_id").stdout.strip()


def main(base, package):
    with tempfile.TemporaryDirectory(prefix="psios-guest-") as work:
        work = Path(work)
        key = work / "admin"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
        instances = [work / "first", work / "second"]
        for instance in instances:
            subprocess.run([sys.executable, ROOT / "images/qemu/create.py", "--base", base, "--package", package,
                            "--authorized-key", key.with_suffix(".pub"), "--output", instance], check=True)
        with boot(instances[0], key, 1) as run:
            node, machine, original_boot = identity(run)
            receipt = json.loads(run("echo '{}' | " + CTL + " submit system.status --key boot-acceptance").stdout)
            task = receipt["task_id"]
            assert json.loads(run(CTL + " wait " + task).stdout)["lifecycle"] == "Succeeded"
        with boot(instances[0], key, 2) as run:
            again, same_machine, new_boot = identity(run)
            assert (again, same_machine) == (node, machine) and original_boot != new_boot
            assert json.loads(run(CTL + " wait " + task).stdout)["lifecycle"] == "Succeeded"
        with boot(instances[1], key, 1) as run:
            other, other_machine, _ = identity(run)
            assert other != node and other_machine != machine
        print("PASS two independent Linux guests, SSH administrator login, offline provisioning, real reboot and durable Task")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    args = parser.parse_args()
    main(args.base.resolve(), args.package.resolve())
