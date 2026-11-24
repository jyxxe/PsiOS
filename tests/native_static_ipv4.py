"""Boot a real native static IPv4 profile on an isolated TAP without DHCP."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_dhcp import MAC, OTHER, wait_for
from native_dhcp_routes import TAP, has, local_link, lookup, routes
from native_disk import (
    REPO, boot, call, check_clean_shutdown, extract_partition, password, replace_partition, sha,
)

CONFIG = "/etc/psios/static-ipv4"


def installer(args, output, archive, program=None):
    return [sys.executable, program or args.installer, output, "--kernel-iso", args.kernel_iso,
            "--core-archive", archive, "--core-sha256", sha(archive)]


def ipv4_guest(console, dns):
    wait_for(console, "grep -Fxq 'Static 192.0.2.20/24 on eth0' /run/psios-network/status")
    assert "192.0.2.20/24" in console.run("busybox ip -4 addr show dev eth0")
    assert has(console, "default", "192.0.2.2", metric=30320), routes(console)
    assert console.run("cat /etc/resolv.conf").splitlines() == [f"nameserver {name}" for name in dns]
    console.run("! pidof udhcpc && test ! -e /etc/psios/dhcp-mac && "
                "test \"$(cat /proc/sys/net/ipv6/conf/eth0/disable_ipv6)\" = 1")


def poweroff(console):
    console.send("poweroff")
    assert console.vm.wait(35) == 0


def refuse_corrupt_static(args, work, image, previous, upgraded):
    damaged = work / "damaged.raw"
    subprocess.run(["qemu-img", "convert", "-f", "raw", "-O", "raw", image, damaged], check=True, timeout=30)
    part = work / "damaged.ext4"
    extract_partition(damaged, part, 65, 192)
    profile = work / "invalid-ipv4"
    profile.write_text(f"{MAC}\n192.0.2.20/24\n203.0.113.1\n")
    profile.chmod(0o600)
    commands = (f"rm {CONFIG}\nwrite {profile} {CONFIG}\nset_inode_field {CONFIG} mode 0100600\n")
    subprocess.run(["debugfs", "-w", "-f", "-", part], input=commands, text=True,
                   stdout=subprocess.DEVNULL, check=True, timeout=15)
    replace_partition(damaged, part, 65)
    source = sha(damaged)
    refused = subprocess.run([*installer(args, upgraded, args.archive),
                              "--upgrade-from", damaged, "--previous-receipt", previous],
                             capture_output=True, text=True, timeout=150)
    assert refused.returncode and "invalid persisted static IPv4 profile" in refused.stderr, refused.stderr
    assert not upgraded.exists() and sha(damaged) == source
    damaged.unlink()
    part.unlink()


def exercise(args, work):
    local_link()
    operator, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    for name, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / name, work / target)
    image = work / "static.raw"
    install = [*installer(args, image, args.previous_archive), "--console-hash", operator_hash,
               "--root-console-hash", root_hash]
    flags = ["--static-mac", MAC, "--static-address", "192.0.2.20/24", "--static-gateway", "192.0.2.2",
             "--static-dns", "192.0.2.3", "--static-dns", "192.0.2.4"]
    for params, error in [
        (["--static-address", "192.0.2.20/24"], "require --static-mac"),
        (["--static-mac", MAC, "--static-address", "192.0.2.20/24", "--static-gateway", "203.0.113.1"], "different host"),
        (["--dhcp-mac", MAC, *flags], "not allowed with argument"),
        ([*flags, "--static-dns", "192.0.2.30", "--static-dns", "192.0.2.31"], "at most three"),
    ]:
        refused = subprocess.run([*install, *params], capture_output=True, text=True)
        assert refused.returncode != 0 and error in refused.stderr and not image.exists(), refused
    receipt = json.loads(subprocess.check_output([*install, *flags]))
    assert receipt["format"] == 4 and receipt["network_profile"] == "virtio-static-ipv4", receipt
    assert sha(image) == receipt["initial_image_sha256"]
    with boot(work, image, network_mac=MAC, tap=TAP) as c:
        c.login("psios", operator)
        ipv4_guest(c, ["192.0.2.3", "192.0.2.4"])
        identity = json.loads(c.run("psiosctl whoami"))["authority_id"]
        assert json.loads(c.run("cat /psios-deployment.json"))["network_profile"] == receipt["network_profile"]
        c.send("exit")
        c.login("root", root)
        assert c.run(f"stat -c '%u:%g:%a' {CONFIG}") == "0:0:600"
        c.run("busybox ping -c 1 -W 2 192.0.2.2 >/dev/null && busybox ping -c 1 -W 2 198.51.100.7 >/dev/null")

        # Compete for the default metric after the supported profile has been applied.
        c.run("busybox ip -4 route del default via 192.0.2.2 dev eth0 proto 4 metric 30320 && "
              "busybox ip -4 addr del 192.0.2.20/24 dev eth0 && "
              "busybox ip -4 addr add 192.0.2.60/24 dev eth0 && "
              "busybox ip -4 route add default via 192.0.2.4 dev eth0 proto 4 metric 30320")
        c.run("/bin/sh /psios-dhcp static >/tmp/static-apply.log 2>&1; test $? -ne 0")
        assert c.run("cat /run/psios-network/status") == "Could not apply static IPv4 on eth0"
        report = c.run("busybox ip -4 addr show dev eth0")
        assert "192.0.2.20/24" not in report and "192.0.2.60/24" in report, report
        assert has(c, "default", "192.0.2.4", metric=30320)
        assert c.run("cat /etc/resolv.conf").splitlines() == ["nameserver 192.0.2.3", "nameserver 192.0.2.4"]
        c.run(f"printf '%s\\n' {MAC} 192.0.2.20/24 203.0.113.1 > {CONFIG}")
        c.run("/bin/sh /psios-dhcp static >/tmp/static-apply.log 2>&1; test $? -ne 0")
        assert c.run("cat /run/psios-network/status") == "invalid static IPv4 profile"
        assert "192.0.2.20/24" not in c.run("busybox ip -4 addr show dev eth0")
        assert has(c, "default", "192.0.2.4", metric=30320)
        c.run(f"printf '%s\\n' {MAC} 192.0.2.20/24 192.0.2.2 192.0.2.53 > {CONFIG}")
        poweroff(c)
    check_clean_shutdown(image)

    previous = work / "previous.json"
    previous.write_text(json.dumps(receipt))
    upgraded = work / "upgraded.raw"
    source = sha(image)
    upgrade = ["--upgrade-from", image, "--previous-receipt", previous]
    older = subprocess.run([*installer(args, upgraded, args.archive, args.previous_installer), *upgrade],
                           capture_output=True, text=True)
    assert older.returncode and "unsupported previous build receipt format" in older.stderr, older
    assert not upgraded.exists() and sha(image) == source
    refuse_corrupt_static(args, work, image, previous, upgraded)
    current = json.loads(subprocess.check_output([*installer(args, upgraded, args.archive), *upgrade]))
    assert current["format"] == 4 and current["network_profile"] == receipt["network_profile"]
    assert current["source_commit"] != receipt["source_commit"]
    assert current["upgrade"]["verified_authority"]["node_id"] == identity and sha(image) == source
    with boot(work, upgraded, network_mac=MAC, tap=TAP) as c:
        c.login("psios", operator)
        ipv4_guest(c, ["192.0.2.53"])
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
        grant = call(c, "Grant.Issue", {"principal_id": "local-owner", "units": 1, "ttl_seconds": 120,
                     "actions": ["task.submit", "task.read", "node.route_lookup"], "tool_targets": [identity]})
        lookup(c, grant["delegation_ref"]["resource_id"], "static-route", "192.0.2.2")
        c.send("exit")
        c.login("root", root)
        poweroff(c)
    check_clean_shutdown(upgraded)

    with boot(work, upgraded, network_mac=OTHER, tap=TAP) as c:
        c.login("psios", operator)
        wait_for(c, "grep -Fxq 'configured virtio interface is absent' /run/psios-network/status")
        wait_for(c, f"test \"$(cat /sys/class/net/eth0/address 2>/dev/null)\" = {OTHER}")
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
        c.run("! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && "
              "test -z \"$(busybox ip -4 route show default)\" && ! pidof udhcpc")
        c.send("exit")
        c.login("root", root)
        poweroff(c)
    check_clean_shutdown(upgraded)
    print("PASS native static IPv4, scoped traffic, route-conflict rollback, corrupt config refusal, format-4 upgrade and wrong MAC")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for arg in ("--kernel-iso", "--archive", "--previous-archive", "--previous-installer"):
        parser.add_argument(arg, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-static-ipv4-") as directory:
        exercise(parser.parse_args(), Path(directory))
