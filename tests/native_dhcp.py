"""Boot an installed UEFI disk with real DHCP inside a private host network namespace."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, password, sha

MAC = "52:54:00:51:6f:20"
OTHER = "52:54:00:51:6f:21"
IPV6_PREFIX = "2001:db8:70::/64"
IPV6_ADDRESS = "2001:db8:70::5054:ff:fe51:6f20"


def wait_for(c, command):
    return c.run(f"for i in $(seq 1 100); do ( {command} ) && exit 0; sleep 0.1; done; exit 1")


def check_lease(c):
    try:
        wait_for(c, "busybox ip -4 addr show dev eth0 | grep -Fq '192.0.2.20/24' && "
                    "busybox ip -4 route show default | grep -Eq '^default via 192[.]0[.]2[.]2 dev eth0 +metric 30310 *$' && "
                    "grep -qx 'nameserver 192.0.2.3' /etc/resolv.conf && "
                    "grep -Fxq 'Bound 192.0.2.20/24 on eth0' /run/psios-network/status")
    except AssertionError as failure:
        observed = c.run("busybox ip -4 addr show dev eth0; busybox ip -4 route show; cat /etc/resolv.conf; true")
        raise AssertionError(observed) from failure


def check_slaac(c):
    wait_for(c, 'flags=$(awk \'$1 == "20010db800700000505400fffe516f20" && $6 == "eth0" {print $5}\' '
                '/proc/net/if_inet6); test -n "$flags" && test "$((0x$flags & 72))" -eq 0')
    assert f"{IPV6_ADDRESS}/64" in c.run("busybox ip -6 addr show dev eth0")
    assert "default via fe80::2 dev eth0" in c.run("busybox ip -6 route show default")
    assert c.run("cat /proc/sys/net/ipv6/conf/eth0/accept_redirects") == "0"


def prepare(args, work, *, dhcp_driver="virtio", dual_stack=False):
    for name, source in (("code.fd", "OVMF_CODE_4M.fd"), ("vars.fd", "OVMF_VARS_4M.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / name)
    user_secret, user_hash = password(work, "operator.hash")
    root_secret, root_hash = password(work, "root.hash")
    image = work / "network.raw"
    build = [sys.executable, args.installer, image, "--kernel-iso", args.kernel_iso,
             "--core-archive", args.archive, "--core-sha256", sha(args.archive),
             "--console-hash", user_hash, "--root-console-hash", root_hash, "--dhcp-mac"]
    refused = subprocess.run(build + ["ff:ff:ff:ff:ff:ff"], capture_output=True, text=True, timeout=10)
    assert refused.returncode and "unicast MAC" in refused.stderr and not image.exists(), refused.stderr
    profile = ["--dhcp-slaac"] if dual_stack else []
    if dual_stack:
        missing_mac = subprocess.run([*build[:-1], *profile], capture_output=True, text=True, timeout=10)
        assert missing_mac.returncode == 2 and "--dhcp-slaac requires --dhcp-mac" in missing_mac.stderr
        assert not image.exists()
    data = subprocess.check_output([*build, MAC, "--dhcp-driver", dhcp_driver, *profile], timeout=150)
    receipt = json.loads(data)
    if dual_stack:
        assert (receipt["format"], receipt["network_profile"]) == (13, "dhcp-slaac"), receipt
        assert receipt.get("wired_driver") == ("e1000e" if dhcp_driver == "e1000e" else None)
        previous = work / "network.json"
        previous.write_bytes(data)
        with tempfile.TemporaryDirectory(dir=work) as stage:
            verification = ("from pathlib import Path; import sys; from install_source import stage_image\n"
                            "work, image, receipt = map(Path, sys.argv[1:4])\n"
                            "_, facts = stage_image(work, image, receipt, sys.argv[4])\n"
                            "assert (facts['receipt_format'], facts['network_profile']) == (13, 'dhcp-slaac')\n")
            subprocess.run([sys.executable, "-B", "-c", verification, stage, image, previous, sha(previous)],
                           cwd=args.installer.resolve().parent, check=True, timeout=90)
    elif dhcp_driver == "e1000e":
        assert (receipt["format"], receipt["wired_driver"]) == (8, "e1000e"), receipt
    else:
        assert (receipt["format"], receipt.get("wired_driver")) == (2, None), receipt
    return image, user_secret, root_secret


def check_dual_upgrade(args, work, image, guest, user_secret, root_secret, identity, task, result):
    staged = work / "staged"
    previous = work / "network.json"
    code, variables = work / "code.fd", work / "vars.fd"
    source_digest, firmware_digest = sha(image), sha(variables)
    report = json.loads(subprocess.check_output([sys.executable, args.installer.with_name("stage_update.py"), staged,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive),
        "--upgrade-from", image, "--previous-receipt", previous, "--previous-receipt-sha256", sha(previous),
        "--ovmf-code", code, "--ovmf-code-sha256", sha(code),
        "--ovmf-vars", variables, "--ovmf-vars-sha256", firmware_digest], timeout=480))
    assert report["status"] == "Staged" and (sha(image), sha(variables)) == (source_digest, firmware_digest)
    receipt = json.loads((staged / "candidate-build.json").read_bytes())
    assert (receipt["format"], receipt["network_profile"]) == (13, "dhcp-slaac"), receipt
    with boot(work, staged / "candidate.raw", **guest) as c:
        c.login("psios", user_secret)
        check_lease(c)
        check_slaac(c)
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
        assert call(c, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        c.send("exit")
        c.login("root", root_secret)
        c.send("cd /; exec poweroff")
        assert c.vm.wait(35) == 0
    check_clean_shutdown(staged / "candidate.raw")


def exercise(args, work):
    image, user_secret, root_secret = prepare(args, work, dhcp_driver=args.dhcp_driver, dual_stack=args.dual_stack)
    model, driver = ("e1000e", "e1000e") if args.dhcp_driver == "e1000e" else ("virtio-net-pci", "virtio_net")
    guest = dict(network_mac=MAC, network_model=model, pace_commands=True)
    if args.dual_stack:
        guest["ipv6_network"] = IPV6_PREFIX
    with boot(work, image, **guest) as c:
        c.login("psios", user_secret)
        identity = json.loads(c.run("psiosctl whoami"))["authority_id"]
        check_lease(c)
        assert c.run("basename $(readlink /sys/class/net/eth0/device/driver)") == driver
        c.run("test \"$(cat /sys/class/net/eth0/address)\" = " + MAC)
        if args.dual_stack:
            check_slaac(c)
            task = json.loads(c.run("printf '{}' | psiosctl submit system.status --key native-dhcp-slaac"))["task_id"]
            result = call(c, "Task.Wait", {"id": task, "timeout_ms": 5000})
            assert result["lifecycle"] == "Succeeded"
        else:
            c.run("test \"$(cat /proc/sys/net/ipv6/conf/eth0/disable_ipv6)\" = 1")
        c.send("exit")
        c.login("root", root_secret)
        before = c.run("stat -Lc %i /etc/resolv.conf")
        c.run("kill -USR1 $(pidof udhcpc)")
        wait_for(c, f"test \"$(stat -Lc %i /etc/resolv.conf)\" != {before}")
        check_lease(c)
        c.run("kill -USR2 $(pidof udhcpc)")
        wait_for(c, "! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && "
                    "test -z \"$(busybox ip -4 route show default)\" && test ! -s /etc/resolv.conf && "
                    "grep -Fxq 'No active lease on eth0' /run/psios-network/status")
        if args.dual_stack:
            check_slaac(c)
            c.run("busybox ping6 -c 1 -W 3 -I eth0 fe80::2")
        c.run("kill -USR1 $(pidof udhcpc)")
        check_lease(c)
        if args.dual_stack:
            check_slaac(c)
        c.send("poweroff")
        assert c.vm.wait(30) == 0
    check_clean_shutdown(image)

    with boot(work, image, **{**guest, "network_mac": OTHER}) as c:
        c.login("psios", user_secret)
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
        wait_for(c, f"test \"$(cat /sys/class/net/eth0/address 2>/dev/null)\" = {OTHER}")
        wait_for(c, f"grep -Fxq 'configured {args.dhcp_driver} interface is absent' /run/psios-network/status")
        c.run("test ! -s /etc/resolv.conf && ! busybox ip -4 addr show dev eth0 | grep -q 'inet '")
        c.run("test -z \"$(busybox ip -4 route show default)\"")
        if args.dual_stack:
            c.run("! busybox ip -6 addr show dev eth0 | grep -q '2001:db8:70:' && "
                  "test -z \"$(busybox ip -6 route show default)\"")
        c.send("exit")
        c.login("root", root_secret)
        c.send("poweroff")
        assert c.vm.wait(30) == 0
    check_clean_shutdown(image)
    if args.dual_stack:
        check_dual_upgrade(args, work, image, guest, user_secret, root_secret, identity, task, result)
        print(f"Verified {driver} DHCPv4 and IPv6 SLAAC kept independent routes through release and staged upgrade.")
    else:
        print(f"Verified {driver} DHCP address, scoped default route and DNS through renewal, release, rebind and wrong-MAC boot.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--dhcp-driver", choices=("virtio", "e1000e"), default="virtio")
    parser.add_argument("--dual-stack", action="store_true")
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-dhcp-test-") as directory:
        exercise(args, Path(directory))
