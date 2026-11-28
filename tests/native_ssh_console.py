"""Exercise native SSH custody and the authorized operator in actual OVMF guests."""
import argparse
import json
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile

from native_dhcp import MAC, check_lease
from native_disk import REPO, boot, call, check_clean_shutdown, password, sha


def client_key(work, name):
    key = work / name
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True, timeout=10)
    return key


def connect(port, trusted, login, key, command, *, extra=()):
    return subprocess.run(["ssh", "-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
                           "-o", "IdentityAgent=none", "-o", "StrictHostKeyChecking=yes",
                           "-o", f"UserKnownHostsFile={trusted}", "-o", "ConnectTimeout=5",
                           "-p", str(port), "-i", key, *extra, f"{login}@127.0.0.1", command],
                          capture_output=True, text=True, timeout=15)


def serial_host_key(console):
    value = console.run("for n in $(seq 1 100); do test -s /run/psios-ssh/host-key.pub && break; "
                        "sleep 0.1; done; cat /run/psios-ssh/host-key.pub")
    key_type, key, *_ = value.split()
    assert key_type == "ssh-ed25519"
    return f"{key_type} {key}"


def shutdown(console, admin, image):
    console.send("exit")
    console.login("root", admin)
    console.send("cd /; exec poweroff")
    assert console.vm.wait(35) == 0
    check_clean_shutdown(image)


def exercise(args, work):
    for name, source in (("code.fd", "OVMF_CODE_4M.fd"), ("vars.fd", "OVMF_VARS_4M.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / name)
    operator, console_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    key = client_key(work, "operator")
    unknown = client_key(work, "unknown")
    auth = key.with_suffix(".pub")
    common = ["--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive)]
    factory = ["--console-hash", console_hash, "--root-console-hash", root_hash, "--dhcp-mac", MAC]
    image = work / "ssh.raw"
    refused_path = work / "invalid.raw"
    malformed = work / "invalid.pub"
    malformed.write_text("command=\"id\" " + auth.read_text())
    for params in (("--console-hash", console_hash, "--ssh-authorized-key", auth),
                   (*factory, "--ssh-authorized-key", malformed)):
        refused = subprocess.run([sys.executable, args.installer, refused_path, *common, *params],
                                 capture_output=True, text=True, timeout=10)
        assert refused.returncode == 1 or refused.returncode == 2, refused.stderr
        assert not refused_path.exists()
    raw = subprocess.check_output([sys.executable, args.installer, image, *common, *factory,
                                   "--ssh-authorized-key", auth], timeout=160)
    receipt = json.loads(raw)
    assert (receipt["format"], receipt["ssh_console"]) == (9, "Ed25519Operator"), receipt
    receipt_path = work / "ssh.json"
    receipt_path.write_bytes(raw)
    stage = work / "install-source"
    stage.mkdir()
    verification = ("from pathlib import Path; import sys; from install_source import stage_image\n"
                    "work, image, receipt = map(Path, sys.argv[1:4])\n"
                    "_, facts = stage_image(work, image, receipt, sys.argv[4])\n"
                    "assert facts['ssh_console'] == 'Ed25519Operator'\n")
    subprocess.run([sys.executable, "-B", "-c", verification, stage, image, receipt_path, sha(receipt_path)],
                   cwd=args.installer.resolve().parent, check=True, timeout=90)
    shutil.rmtree(stage)
    clone = work / "clone.raw"
    subprocess.run(["cp", "--sparse=always", image, clone], check=True)

    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    trusted = work / "known_hosts"
    with boot(work, image, network_mac=MAC, ssh_port=port, pace_commands=True) as console:
        console.login("psios", operator)
        check_lease(console)
        host_key = serial_host_key(console)
        trusted.write_text(f"[127.0.0.1]:{port} {host_key}\n")
        remote = connect(port, trusted, "psios", key, "psiosctl node")
        assert remote.returncode == 0, remote.stderr
        node = json.loads(remote.stdout)["reference"]["resource_id"]
        assert node == call(console, "Node.Get", {})["reference"]["resource_id"]
        for account, credential, options in (("root", key, ()), ("psios", unknown, ()),
                                             ("psios", key, ("-o", "PreferredAuthentications=password"))):
            denied = connect(port, trusted, account, credential, "id", extra=options)
            assert denied.returncode == 255 and "Permission denied (publickey)" in denied.stderr, denied.stderr
        tty = connect(port, trusted, "psios", key, "tty", extra=("-tt",))
        assert tty.returncode == 0 and tty.stdout.startswith("/dev/pts/"), tty
        task = json.loads(console.run("printf '{}' | psiosctl submit system.status --key ssh-persistence"))["task_id"]
        result = call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})
        assert result["lifecycle"] == "Succeeded"
        console.run("printf 'persisted SSH home\\n' > ~/ssh-persistence; sync")
        console.vm.kill()
        console.vm.wait(15)

    with boot(work, image, network_mac=MAC, ssh_port=port, pace_commands=True) as console:
        console.login("psios", operator)
        check_lease(console)
        assert serial_host_key(console) == host_key
        remote = connect(port, trusted, "psios", key, "cat ~/ssh-persistence")
        assert (remote.returncode, remote.stdout) == (0, "persisted SSH home\n"), remote.stderr
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        shutdown(console, admin, image)

    with boot(work, clone, network_mac=MAC, pace_commands=True) as console:
        console.login("psios", operator)
        assert serial_host_key(console) != host_key
        shutdown(console, admin, clone)
    clone.unlink()

    original = sha(image)
    if args.staged:
        candidate = work / "staged"
        code, variables = work / "code.fd", work / "vars.fd"
        firmware_digest = sha(variables)
        raw = subprocess.check_output([sys.executable, args.installer.with_name("stage_update.py"), candidate, *common,
                                       "--upgrade-from", image, "--previous-receipt", receipt_path,
                                       "--previous-receipt-sha256", sha(receipt_path), "--ovmf-code", code,
                                       "--ovmf-code-sha256", sha(code), "--ovmf-vars", variables,
                                       "--ovmf-vars-sha256", firmware_digest], timeout=480)
        assert json.loads(raw)["status"] == "Staged" and sha(variables) == firmware_digest
        upgraded = candidate / "candidate.raw"
        observed = json.loads((candidate / "candidate-build.json").read_bytes())
    else:
        upgraded = work / "upgraded.raw"
        raw = subprocess.check_output([sys.executable, args.installer, upgraded, *common,
                                       "--upgrade-from", image, "--previous-receipt", receipt_path], timeout=360)
        observed = json.loads(raw)
    assert observed["ssh_console"] == receipt["ssh_console"] and sha(image) == original
    with boot(work, upgraded, network_mac=MAC, ssh_port=port, pace_commands=True) as console:
        console.login("psios", operator)
        check_lease(console)
        assert serial_host_key(console) == host_key
        remote = connect(port, trusted, "psios", key, "psiosctl node")
        assert remote.returncode == 0, remote.stderr
        assert json.loads(remote.stdout)["reference"]["resource_id"] == node
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        shutdown(console, admin, upgraded)

    print("Verified native SSH authentication and PTY, unique clone keys, and key and Core continuity after a cut and offline upgrade.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-ssh-") as directory:
        exercise(args, Path(directory))
