"""Boot two firmware profiles and check persisted state after shutdown and abrupt guest loss."""
import argparse
import contextlib
from datetime import datetime
import json
import hashlib
import shlex
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

from model_leases import open_session


ROOT = Path(__file__).resolve().parents[1]
CTL = "sudo -n -u psios /usr/bin/psiosctl"


@contextlib.contextmanager
def boot(instance, key, number, *, power_cut=False):
    # Bind forwarding only on loopback. restrict=on prevents guest access to
    # external package mirrors, registries or model endpoints during acceptance.
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    serial = instance / f"serial-{number}.log"
    with subprocess.Popen([sys.executable, ROOT / "images/qemu/run.py", instance, "--serial-log", serial,
                           "--ssh-port", str(port)]) as vm:
        ssh = ["ssh", "-T", "-p", str(port), "-i", str(key), "-o", "BatchMode=yes",
               "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=4",
               "-o", "HostKeyAlias=" + instance.name, "-o", "UserKnownHostsFile=" + str(instance / "known_hosts"),
               "psios-admin@127.0.0.1"]

        def run(command, *, check=True, timeout=40, data=None):
            return subprocess.run(ssh + [command], check=check, input=data, text=True, capture_output=True, timeout=timeout)
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
            if power_cut:
                vm.kill()
                if vm.wait(timeout=10) != -signal.SIGKILL:
                    raise RuntimeError("guest exited before injected power loss")
                return
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


@contextlib.contextmanager
def guest_clock(run, at):
    original = int(run("date +%s").stdout)
    start = time.monotonic()
    run("sudo -n date -u -s @" + str(at))
    try:
        yield
    finally:
        run("sudo -n date -u -s @" + str(original + int(time.monotonic() - start)))


def model_call(run, method, params, *, fenced=False):
    reply = run(CTL + " call " + method, data=json.dumps(params), check=not fenced)
    if fenced:
        assert reply.returncode != 0 and "Fenced" in reply.stderr, reply
    else:
        return json.loads(reply.stdout)


def clock_changes(run):
    def call(method, params):
        return model_call(run, method, params)
    long = open_session(call, "long-clock-check")
    short = open_session(call, "short-clock-check", seconds=1)
    claim = dict(session_id=long, key="before-boot")
    granted = call("Model.Claim", claim)
    now = int(run("date +%s").stdout)
    with guest_clock(run, now + 600):
        recovered = call("Model.Claim", claim)
        assert recovered["lease_id"] == granted["lease_id"] and recovered["recovered"]
    short_call = dict(session_id=short, key="short")
    short_lease = call("Model.Claim", short_call)
    with guest_clock(run, now - 600):
        time.sleep(1.1)
        model_call(run, "Model.Respond", dict(session_id=short, lease_id=short_lease["lease_id"],
                   decision=dict(action="finish", summary="late")), fenced=True)
    return claim, granted


def verify_management_address(run, node):
    scope = {"principal_id": "local-owner", "actions": ["task.submit", "task.read", "node.interfaces"],
             "tool_targets": [node], "units": 1, "ttl_seconds": 300}
    grant = json.loads(run(CTL + " call Grant.Issue", data=json.dumps(scope)).stdout)["delegation_ref"]["resource_id"]
    receipt = json.loads(run(CTL + " submit node.interfaces --key boot-interfaces --delegation " + shlex.quote(grant),
                             data="{}").stdout)
    result = json.loads(run(CTL + " wait " + shlex.quote(receipt["task_id"])).stdout)
    sample = json.loads(run(CTL + " call Artifact.Read",
                            data=json.dumps({"reference": result["result"]["artifact_refs"][0]})).stdout)
    server_ip = run('printf "%s" "$SSH_CONNECTION"').stdout.split()[2]
    assert any(a["local"] == server_ip for i in sample["interfaces"] for a in i["addr_info"]), sample


def main(base, package):
    with tempfile.TemporaryDirectory(prefix="psios-guest-") as work:
        work = Path(work)
        key = work / "admin"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
        instances = [work / "first", work / "second"]
        for instance, firmware in zip(instances, ("bios", "uefi"), strict=True):
            subprocess.run([sys.executable, ROOT / "images/qemu/create.py", "--base", base, "--package", package,
                            "--authorized-key", key.with_suffix(".pub"), "--output", instance, "--firmware", firmware], check=True)
        with boot(instances[0], key, 1) as run:
            node, machine, original_boot = identity(run)
            run("test ! -d /sys/firmware/efi")
            verify_management_address(run, node)
            receipt = json.loads(run("echo '{}' | " + CTL + " submit system.status --key boot-acceptance").stdout)
            task = receipt["task_id"]
            assert json.loads(run(CTL + " wait " + task).stdout)["lifecycle"] == "Succeeded"
        with boot(instances[0], key, 2) as run:
            again, same_machine, new_boot = identity(run)
            assert (again, same_machine) == (node, machine) and original_boot != new_boot
            assert json.loads(run(CTL + " wait " + task).stdout)["lifecycle"] == "Succeeded"
        uefi = instances[1]
        manifest = json.loads((uefi / "manifest.json").read_text())
        with (uefi / "uefi-vars.fd").open("rb") as f:
            assert hashlib.file_digest(f, "sha256").hexdigest() == manifest["ovmf"]["initial_vars_sha256"]
        with boot(uefi, key, 1, power_cut=True) as run:
            other, other_machine, before_cut = identity(run)
            assert other != node and other_machine != machine
            run("test -d /sys/firmware/efi/efivars")
            # A nonvolatile EFI variable belongs to this VM, independently of the system disk.
            marker = "/sys/firmware/efi/efivars/PsiOSAcceptance-6ecbc2fa-bbb1-4b43-b71a-a98d6b50fd89"
            run("sudo -n python3 -c " + shlex.quote(
                f"from pathlib import Path; Path('{marker}').write_bytes(bytes([7, 0, 0, 0]) + b'psios')"))
            receipt = json.loads(run(CTL + " submit system.status --key before-power-cut", data="{}").stdout)
            prior = json.loads(run(CTL + " wait " + shlex.quote(receipt["task_id"])).stdout)
            assert prior["lifecycle"] == "Succeeded"
            claim, lease = clock_changes(run)
        with boot(uefi, key, 2) as run:
            same, machine_after_cut, after_cut = identity(run)
            assert (same, machine_after_cut) == (other, other_machine) and after_cut != before_cut
            run("sudo -n python3 -c " + shlex.quote(
                f"from pathlib import Path; assert Path('{marker}').read_bytes() == bytes([7, 0, 0, 0]) + b'psios'"))
            replay = json.loads(run(CTL + " submit system.status --key before-power-cut", data="{}").stdout)
            assert replay["task_id"] == receipt["task_id"]
            recovered = json.loads(run(CTL + " wait " + shlex.quote(replay["task_id"])).stdout)
            assert recovered["lifecycle"] == "Succeeded" and recovered["result"] == prior["result"]
            # Keep UTC inside the former lease even on a slow software-emulated boot.
            within_old_lease = int(datetime.fromisoformat(lease["expires_at"].replace("Z", "+00:00")).timestamp()) - 60
            with guest_clock(run, within_old_lease):
                model_call(run, "Model.Claim", claim, fenced=True)
                model_call(run, "Model.Respond", dict(session_id=claim["session_id"], lease_id=lease["lease_id"],
                           decision=dict(action="finish", summary="previous boot")), fenced=True)
                fresh = model_call(run, "Model.Claim", dict(session_id=claim["session_id"], key="after-boot"))
                assert fresh["turn"] == 2 and fresh["lease_id"] != lease["lease_id"]
        code = uefi / "uefi-code.fd"
        code.chmod(0o600)
        with code.open("r+b") as f:
            first = f.read(1)
            f.seek(0)
            f.write(bytes([first[0] ^ 1]))
        rejected = subprocess.run([sys.executable, ROOT / "images/qemu/run.py", uefi],
                                  capture_output=True, text=True, timeout=5)
        assert rejected.returncode != 0 and "OVMF code differs" in rejected.stderr, rejected.stderr
        print("PASS independent BIOS and UEFI guests, persistent OVMF variables, warm restart, "
              "abrupt VM loss, durable Task replay, guest UTC jumps and boot-bound model leases")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    args = parser.parse_args()
    main(args.base.resolve(), args.package.resolve())
