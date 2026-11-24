"""Boot native IPv6 and dual-stack profiles on a real isolated Linux TAP."""
import argparse
import ipaddress
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

from native_dhcp import MAC, wait_for
from native_dhcp_routes import TAP, has, local_link
from native_disk import REPO, boot, check_clean_shutdown, extract_partition, password, replace_partition, sha
from native_static_ipv4 import installer, poweroff


ADDRESS = ipaddress.IPv6Interface("2001:db8:70::20/64")
GATEWAY = ipaddress.IPv6Address("fe80::2")
DESTINATION = "2001:db8:99::7"
DNS6 = ipaddress.IPv6Address("2001:db8:70::53")
DNS4 = "192.0.2.53"
CONFIG = "/etc/psios/static-ipv6"


def host_ipv6():
    local_link()
    for device, address in [(TAP, f"{GATEWAY}/64"), (TAP, "2001:db8:70::2/64"), ("lo", DESTINATION + "/128")]:
        subprocess.run(["ip", "-6", "addr", "add", address, "dev", device, "nodad"], check=True, timeout=8)


def wire_address():
    return f"{ADDRESS.ip.exploded}/{ADDRESS.network.prefixlen}"


def ready(console, dual=False):
    description = ("192.0.2.20/24 and " if dual else "") + wire_address()
    wait_for(console, f"grep -Fxq 'Static {description} on eth0' /run/psios-network/status")
    assert str(ADDRESS) in console.run("busybox ip -6 addr show dev eth0")
    routes = console.run("busybox ip -6 route show dev eth0").splitlines()
    assert any(f"default via {GATEWAY}" in route and "metric 30320" in route for route in routes), routes
    nameservers = ([DNS4] if dual else []) + [DNS6.exploded]
    assert console.run("cat /etc/resolv.conf").splitlines() == [f"nameserver {ip}" for ip in nameservers]
    assert console.run("cat /proc/sys/net/ipv6/conf/eth0/accept_ra /proc/sys/net/ipv6/conf/eth0/autoconf "
                       "/proc/sys/net/ipv6/conf/eth0/disable_ipv6 /proc/sys/net/ipv6/conf/eth0/accept_dad").splitlines() == [
        "0", "0", "0", "1"]
    console.run("! pidof udhcpc && test ! -e /etc/psios/dhcp-mac")
    if dual:
        assert has(console, "default", "192.0.2.2", metric=30320)
    else:
        console.run("! busybox ip -4 addr show dev eth0 | grep -q 'inet '")
    return json.loads(console.run("psiosctl whoami"))["authority_id"]


def reach_gateway_destinations(console, dual=False):
    console.run(f"busybox ping6 -c 1 -W 3 {DESTINATION}")
    if dual:
        console.run("busybox ping -c 1 -W 2 198.51.100.7")


def invalid_persisted_ipv6(args, work, image, receipt):
    broken = work / "invalid.raw"
    subprocess.run(["qemu-img", "convert", "-f", "raw", "-O", "raw", image, broken], check=True, timeout=40)
    fs = work / "invalid.ext4"
    extract_partition(broken, fs, 65, 192)
    config = work / "invalid-profile"
    outside = ipaddress.IPv6Address("2001:db8:71::1")
    config.write_text(f"{MAC}\n{wire_address()}\n{outside.exploded}\n")
    config.chmod(0o600)
    subprocess.run(["debugfs", "-w", "-f", "-", fs],
                   input=f"rm {CONFIG}\nwrite {config} {CONFIG}\nset_inode_field {CONFIG} mode 0100600\n",
                   text=True, stdout=subprocess.DEVNULL, check=True, timeout=15)
    replace_partition(broken, fs, 65)
    before = sha(broken)
    output = work / "must-not-be-published.raw"
    rejected = subprocess.run([*installer(args, output, args.archive), "--upgrade-from", broken,
                               "--previous-receipt", receipt], capture_output=True, text=True, timeout=150)
    assert rejected.returncode and "invalid persisted static IPv6 profile" in rejected.stderr, rejected.stderr
    assert not output.exists() and sha(broken) == before


def exercise(args, work):
    host_ipv6()
    operator, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    for name, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / name, work / target)
    image = work / "static6.raw"
    install = [*installer(args, image, args.previous_archive), "--console-hash", operator_hash,
               "--root-console-hash", root_hash]
    flags = ["--static-mac", MAC, "--static-ipv6-address", str(ADDRESS),
             "--static-ipv6-gateway", str(GATEWAY), "--static-ipv6-dns", str(DNS6)]
    for params, error in [
        (flags[2:], "require --static-mac"),
        (["--static-mac", MAC, "--static-ipv6-address", "::ffff:192.0.2.20/120"], "unscoped host address"),
        (["--static-mac", MAC, "--static-ipv6-address", "2001:db8:70::/64"], "unscoped host address"),
        ([*flags[:4], "--static-ipv6-gateway", "2001:db8:71::1"], "different on-link"),
        ([*flags[:6], "--static-ipv6-dns", "fe80::53%2"], "without a zone"),
        ([*flags, "--static-address", "192.0.2.20/24",
          "--static-dns", DNS4, "--static-dns", "192.0.2.4", "--static-dns", "192.0.2.5"], "at most three DNS"),
    ]:
        rejected = subprocess.run([*install, *params], capture_output=True, text=True, timeout=10)
        assert rejected.returncode and error in rejected.stderr and not image.exists(), rejected.stderr

    receipt = json.loads(subprocess.check_output([*install, *flags], timeout=150))
    assert (receipt["format"], receipt["network_profile"]) == (5, "virtio-static-ipv6"), receipt
    assert sha(image) == receipt["initial_image_sha256"]
    with boot(work, image, network_mac=MAC, tap=TAP) as console:
        console.login("psios", operator)
        identity = ready(console)
        manifest = json.loads(console.run("cat /psios-deployment.json"))
        assert manifest["network_profile"] == receipt["network_profile"]
        assert "/psios-ipv6" in manifest["payload_sha256"] and "/psios-static" in manifest["payload_sha256"]
        console.send("exit")
        console.login("root", root)
        assert console.run(f"stat -c '%u:%g:%a:%h' {CONFIG}") == "0:0:600:1"
        reach_gateway_destinations(console)
        poweroff(console)
    check_clean_shutdown(image)
    print("PASS IPv6-only boot and traffic through a link-local gateway", flush=True)

    previous = work / "previous.json"
    previous.write_text(json.dumps(receipt))
    invalid_persisted_ipv6(args, work, image, previous)
    out = work / "upgraded.raw"
    original = sha(image)
    upgraded = json.loads(subprocess.check_output([*installer(args, out, args.archive),
        "--upgrade-from", image, "--previous-receipt", previous], timeout=330))
    assert upgraded["format"] == 5 and upgraded["network_profile"] == receipt["network_profile"]
    assert upgraded["source_commit"] != receipt["source_commit"]
    assert sha(image) == original and upgraded["upgrade"]["verified_authority"]["node_id"] == identity
    with boot(work, out, network_mac=MAC, tap=TAP) as console:
        console.login("psios", operator)
        assert ready(console) == identity
        console.send("exit")
        console.login("root", root)
        reach_gateway_destinations(console)
        poweroff(console)
    check_clean_shutdown(out)
    print("PASS format-5 upgrade and rejection of a malformed saved IPv6 profile", flush=True)

    dual = work / "dual.raw"
    dual_receipt = json.loads(subprocess.check_output([
        *installer(args, dual, args.archive), "--console-hash", operator_hash, "--root-console-hash", root_hash, *flags,
        "--static-address", "192.0.2.20/24", "--static-gateway", "192.0.2.2", "--static-dns", DNS4,
    ], timeout=150))
    assert (dual_receipt["format"], dual_receipt["network_profile"]) == (5, "virtio-static-dual-stack")
    with boot(work, dual, network_mac=MAC, tap=TAP) as console:
        console.login("psios", operator)
        ready(console, dual=True)
        console.send("exit")
        console.login("root", root)
        reach_gateway_destinations(console, dual=True)
        console.run("busybox ip -4 route del default via 192.0.2.2 dev eth0 proto 4 metric 30320 && "
                    "busybox ip -4 addr del 192.0.2.20/24 dev eth0 && "
                    "busybox ip -4 addr add 192.0.2.60/24 dev eth0 && "
                    "busybox ip -4 route add default via 192.0.2.4 dev eth0 proto 4 metric 30320 && "
                    f"busybox ip -6 route del default via {GATEWAY} dev eth0 proto 4 metric 30320 && "
                    f"busybox ip -6 addr del {ADDRESS} dev eth0")
        console.run("/bin/sh /psios-dhcp dual >/tmp/dual-apply.log 2>&1; test $? -ne 0")
        assert console.run("cat /run/psios-network/status") == "Could not apply static dual stack on eth0"
        assert has(console, "default", "192.0.2.4", metric=30320)
        assert "192.0.2.60/24" in console.run("busybox ip -4 addr show dev eth0")
        console.run(f"! busybox ip -6 addr show dev eth0 | grep -Fq '{ADDRESS}' && "
                    "! busybox ip -6 route show dev eth0 | grep -q '^default ' && "
                    "! busybox ip -4 addr show dev eth0 | grep -Fq '192.0.2.20/24'")
        assert console.run("cat /etc/resolv.conf").splitlines() == [f"nameserver {ip}" for ip in (DNS4, DNS6.exploded)]
        poweroff(console)
    check_clean_shutdown(dual)

    subprocess.run(["ip", "-6", "addr", "add", str(ADDRESS), "dev", TAP, "nodad"], check=True, timeout=8)
    with boot(work, dual, network_mac=MAC, tap=TAP) as console:
        console.login("psios", operator)
        wait_for(console, "grep -Fxq 'Could not apply static dual stack on eth0' /run/psios-network/status")
        console.run(f"! busybox ip -6 addr show dev eth0 | grep -Fq '{ADDRESS}' && "
                    "! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && "
                    "test ! -e /run/psios-network/resolv.conf")
        console.send("exit")
        console.login("root", root)
        poweroff(console)
    check_clean_shutdown(dual)
    print("PASS native IPv6 and dual-stack traffic, DAD conflict, two-family rollback and format-5 offline upgrade")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for arg in ("--kernel-iso", "--archive", "--previous-archive"):
        parser.add_argument(arg, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-static-ipv6-") as directory:
        exercise(parser.parse_args(), Path(directory))
