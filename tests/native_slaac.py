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
from native_ssh_console import client_key, serial_host_key
from native_static_ipv4 import installer, poweroff


ADDRESS = IPv6Address("fec0::5054:ff:fe51:6f20")
TAP_ADDRESS = IPv6Address("2001:db8:70::5054:ff:fe51:6f20")
DNS = IPv6Address("fec0::3")
TAP_DNS = IPv6Address("2001:db8:70::53")
CONFIG = "/etc/psios/slaac"


def check_discovery(console, address=ADDRESS, dns=DNS):
    wait_for(console, "grep -Fxq 'IPv6 router discovery enabled on eth0' /run/psios-network/status")
    expected = address.exploded.replace(":", "")
    wait_for(console, f"flags=$(awk '$1 == \"{expected}\" && $6 == \"eth0\" {{print $5}}' /proc/net/if_inet6); "
                      'test -n "$flags" && test "$((0x$flags & 72))" -eq 0')
    assert f"{address}/64" in console.run("busybox ip -6 addr show dev eth0")
    assert "default via fe80::2 dev eth0" in console.run("busybox ip -6 route show default")
    console.run("! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && ! pidof udhcpc")
    assert console.run("cat /etc/resolv.conf") == f"nameserver {dns.exploded}"
    assert console.run("cat /proc/sys/net/ipv6/conf/eth0/accept_redirects") == "0"


def ipv6_login(key, trusted):
    return subprocess.run(["ssh", "-6", "-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
                           "-o", "IdentityAgent=none", "-o", "StrictHostKeyChecking=yes",
                           "-o", f"UserKnownHostsFile={trusted}", "-o", "HostKeyAlias=psios-slaac-test",
                           "-o", "ConnectTimeout=5", "-i", key, f"psios@{TAP_ADDRESS}", "psiosctl node"],
                          capture_output=True, text=True, timeout=15)


def check_dns(console):
    result = console.run("busybox timeout 8 busybox nslookup -type=AAAA node.test; printf 'dns exit %s\\n' \"$?\"")
    assert "2001:db8:70::77" in result and result.endswith("dns exit 0"), result


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
    address = TAP_ADDRESS if args.ssh_tap else ADDRESS
    dns = TAP_DNS if args.ssh_tap else DNS
    guest = dict(network_mac=MAC, pace_commands=True, tap=args.ssh_tap)
    for flags, message in [
        (["--slaac-dns", str(DNS)], "requires --slaac-mac"),
        (["--slaac-mac", MAC, "--slaac-dns", "fe80::53%eth0"], "without a zone"),
        (["--slaac-mac", MAC, "--dhcp-mac", MAC], "not allowed with argument"),
    ]:
        failed = subprocess.run([*install, *flags], capture_output=True, text=True, timeout=10)
        assert failed.returncode and message in failed.stderr and not image.exists(), failed.stderr
    previous = work / "factory.json"
    profile = ["--slaac-mac", MAC, "--slaac-dns", str(dns)]
    if args.ssh_tap:
        key = client_key(work, "operator")
        denied_key = client_key(work, "untrusted")
        trusted = work / "known_hosts"
        profile.extend(("--ssh-authorized-key", key.with_suffix(".pub")))
    previous.write_bytes(subprocess.check_output([*install, *profile], timeout=150))
    receipt = load_receipt(previous)
    assert receipt.get("ssh_console") == ("Ed25519Operator" if args.ssh_tap else None), receipt
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

    with boot(work, image, **guest) as console:
        console.login("psios", operator)
        check_discovery(console, address, dns)
        identity = call(console, "Node.Get", {})["reference"]
        if args.ssh_tap:
            check_dns(console)
            host_key = serial_host_key(console)
            trusted.write_text(f"psios-slaac-test {host_key}\n")
            remote = ipv6_login(key, trusted)
            assert remote.returncode == 0, remote.stderr
            assert json.loads(remote.stdout)["reference"]["resource_id"] == identity["resource_id"]
            denied = ipv6_login(denied_key, trusted)
            assert denied.returncode == 255 and "Permission denied (publickey)" in denied.stderr, denied.stderr
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
    assert load_receipt(updated_receipt).get("ssh_console") == receipt.get("ssh_console")
    candidate = staged / "candidate.raw"
    with boot(work, candidate, **guest) as console:
        console.login("psios", operator)
        check_discovery(console, address, dns)
        assert call(console, "Node.Get", {})["reference"]["resource_id"] == identity["resource_id"]
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        if args.ssh_tap:
            check_dns(console)
            assert serial_host_key(console) == host_key
            remote = ipv6_login(key, trusted)
            assert remote.returncode == 0, remote.stderr
            assert json.loads(remote.stdout)["reference"]["resource_id"] == identity["resource_id"]
        console.send("exit")
        console.login("root", root)
        console.run("busybox ping6 -c 1 -W 3 -I eth0 fe80::2")
        poweroff(console)
    check_clean_shutdown(candidate)

    with boot(work, candidate, **{**guest, "network_mac": OTHER}) as console:
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
    if args.ssh_tap:
        print("Verified IPv6 DNS queries and SSH authentication over TAP before and after staged update.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--ssh-tap", metavar="IFNAME",
                        help="use isolated TAP with 2001:db8:70::/64 advertised from fe80::2 and test IPv6 DNS and SSH")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-slaac-test-") as root:
        exercise(args, Path(root))
