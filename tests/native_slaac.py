"""Discover IPv6 over real router advertisements in an installed native guest."""
import argparse
from ipaddress import IPv6Address
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_dhcp import MAC, OTHER, wait_for
from native_disk import REPO, boot, call, check_clean_shutdown, password, sha
from native_static_ipv4 import installer, poweroff


ADDRESS = IPv6Address("fec0::5054:ff:fe51:6f20")
DNS = IPv6Address("fec0::3")
CONFIG = "/etc/psios/slaac"


def check_discovery(console):
    wait_for(console, "grep -Fxq 'IPv6 router discovery enabled on eth0' /run/psios-network/status")
    expected = ADDRESS.exploded.replace(":", "")
    wait_for(console, f"flags=$(awk '$1 == \"{expected}\" && $6 == \"eth0\" {{print $5}}' /proc/net/if_inet6); "
                      'test -n "$flags" && test "$((0x$flags & 72))" -eq 0')
    assert f"{ADDRESS}/64" in console.run("busybox ip -6 addr show dev eth0")
    assert "default via fe80::2 dev eth0" in console.run("busybox ip -6 route show default")
    console.run("! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && ! pidof udhcpc")
    assert console.run("cat /etc/resolv.conf") == f"nameserver {DNS.exploded}"
    assert console.run("cat /proc/sys/net/ipv6/conf/eth0/accept_redirects") == "0"


def load_receipt(path):
    data = json.loads(path.read_bytes())
    assert (data["format"], data["network_profile"]) == (10, "slaac"), data
    return data


def exercise(args, work):
    for name, destination in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
        shutil.copyfile(args.ovmf_dir / name, work / destination)
    operator, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    image = work / "slaac.raw"
    install = [*installer(args, image, args.archive), "--console-hash", operator_hash, "--root-console-hash", root_hash]
    for flags, message in [
        (["--slaac-dns", str(DNS)], "requires --slaac-mac"),
        (["--slaac-mac", MAC, "--slaac-dns", "fe80::53%eth0"], "without a zone"),
        (["--slaac-mac", MAC, "--dhcp-mac", MAC], "not allowed with argument"),
    ]:
        failed = subprocess.run([*install, *flags], capture_output=True, text=True, timeout=10)
        assert failed.returncode and message in failed.stderr and not image.exists(), failed.stderr
    previous = work / "factory.json"
    previous.write_bytes(subprocess.check_output([*install, "--slaac-mac", MAC, "--slaac-dns", str(DNS)], timeout=150))
    receipt = load_receipt(previous)
    assert sha(image) == receipt["initial_image_sha256"]

    staging = work / "factory-inspection"
    staging.mkdir()
    verify_factory = ("from pathlib import Path; import sys; from install_source import stage_image\n"
                      "work, disk, receipt = map(Path, sys.argv[1:4])\n"
                      "_, facts = stage_image(work, disk, receipt, sys.argv[4])\n"
                      "assert facts['network_profile'] == 'slaac'\n")
    subprocess.run([sys.executable, "-B", "-c", verify_factory, staging, image, previous, sha(previous)],
                   cwd=args.installer.resolve().parent, check=True, timeout=90)
    shutil.rmtree(staging)

    with boot(work, image, network_mac=MAC, pace_commands=True) as console:
        console.login("psios", operator)
        check_discovery(console)
        identity = call(console, "Node.Get", {})["reference"]
        task = json.loads(console.run("printf '{}' | psiosctl submit system.status --key slaac-persist"))["task_id"]
        result = call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})
        assert result["lifecycle"] == "Succeeded"
        console.send("exit")
        console.login("root", root)
        console.run("busybox ping6 -c 1 -W 3 -I eth0 fe80::2")
        assert console.run(f"stat -c '%u:%g:%a:%h' {CONFIG}") == "0:0:600:1"
        poweroff(console)
    check_clean_shutdown(image)
    source = sha(image)

    staged = work / "staged"
    code, variables = work / "code.fd", work / "vars.fd"
    saved_variables = sha(variables)
    stage = installer(args, staged, args.archive, args.installer.with_name("stage_update.py"))
    report = json.loads(subprocess.check_output([*stage, "--upgrade-from", image, "--previous-receipt", previous,
        "--previous-receipt-sha256", sha(previous), "--ovmf-code", code, "--ovmf-code-sha256", sha(code),
        "--ovmf-vars", variables, "--ovmf-vars-sha256", saved_variables], timeout=480))
    assert report["status"] == "Staged" and sha(variables) == saved_variables and sha(image) == source
    updated_receipt = staged / "candidate-build.json"
    load_receipt(updated_receipt)
    candidate = staged / "candidate.raw"
    with boot(work, candidate, network_mac=MAC, pace_commands=True) as console:
        console.login("psios", operator)
        check_discovery(console)
        assert call(console, "Node.Get", {})["reference"]["resource_id"] == identity["resource_id"]
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        console.send("exit")
        console.login("root", root)
        console.run("busybox ping6 -c 1 -W 3 -I eth0 fe80::2")
        poweroff(console)
    check_clean_shutdown(candidate)

    with boot(work, candidate, network_mac=OTHER, pace_commands=True) as console:
        console.login("psios", operator)
        wait_for(console, "grep -Fxq 'configured virtio interface is absent' /run/psios-network/status")
        console.run("! busybox ip -6 addr show dev eth0 | grep -q 'inet6 ' && "
                    "test ! -e /run/psios-network/resolv.conf")
        console.send("exit")
        console.login("root", root)
        console.run(f"chmod 0644 {CONFIG}")
        poweroff(console)
    invalid = sha(candidate)
    refused = work / "refused.raw"
    failed = subprocess.run([*installer(args, refused, args.archive), "--upgrade-from", candidate,
                             "--previous-receipt", updated_receipt], capture_output=True, text=True, timeout=160)
    assert failed.returncode and "invalid persisted SLAAC profile" in failed.stderr, failed.stderr
    assert not refused.exists() and sha(candidate) == invalid and sha(image) == source
    print("Verified native IPv6 router discovery, DAD, gateway traffic, resolver, staged update, wrong MAC and source protection.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-slaac-test-") as root:
        exercise(args, Path(root))
