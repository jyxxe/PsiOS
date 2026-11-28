"""Configure and preserve a real native e1000e static dual-stack link."""
import argparse
import ipaddress
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_dhcp import MAC, OTHER, wait_for
from native_disk import REPO, boot, call, check_clean_shutdown, password, sha

NETWORK = "fd70:5053:494f::/64"
ADDRESS6 = "fd70:5053:494f::20"
GATEWAY6 = "fd70:5053:494f::2"
DNS6 = "fd70:5053:494f::3"


def check_profile(console):
    expanded = ipaddress.IPv6Address(ADDRESS6).exploded
    try:
        wait_for(console, f"grep -Fxq 'Static 192.0.2.20/24 and {expanded}/64 on eth0' /run/psios-network/status")
    except AssertionError as error:
        observation = console.run("cat /run/psios-network/status; busybox ip addr; busybox ip -6 route; true")
        raise AssertionError(observation) from error
    assert console.run("basename $(readlink /sys/class/net/eth0/device/driver)") == "e1000e"
    console.run("busybox ip -4 addr show dev eth0 | grep -F '192.0.2.20/24'")
    console.run(f"busybox ip -6 addr show dev eth0 | grep -F '{ADDRESS6}/64' | grep -v tentative")
    console.run("busybox ip -4 route show default | grep -E '^default via 192[.]0[.]2[.]2 dev eth0 +metric 30320 *$'")
    console.run(f"busybox ip -6 route show default | grep -E '^default via {GATEWAY6} dev eth0 +metric 30320( |$)'")
    resolver = console.run("cat /etc/resolv.conf").splitlines()
    assert resolver == ["nameserver 192.0.2.3", f"nameserver {ipaddress.IPv6Address(DNS6).exploded}"], resolver


def shutdown(console, image):
    console.send("cd /; exec poweroff")
    assert console.vm.wait(35) == 0
    check_clean_shutdown(image)


def exercise(args, work):
    for name, source in (("code.fd", "OVMF_CODE_4M.fd"), ("vars.fd", "OVMF_VARS_4M.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / name)
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    image = work / "static-e1000e.raw"
    source_receipt = work / "static-e1000e.json"
    pinned = ["--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive)]
    build = [sys.executable, "-B", args.installer, image, *pinned, "--console-hash", operator_hash,
             "--root-console-hash", admin_hash, "--static-driver", "e1000e"]
    refused = subprocess.run(build, capture_output=True, text=True, timeout=15)
    assert refused.returncode == 2 and "requires --static-mac" in refused.stderr and not image.exists()
    raw = subprocess.check_output([*build, "--static-mac", MAC, "--static-address", "192.0.2.20/24",
                                   "--static-gateway", "192.0.2.2", "--static-dns", "192.0.2.3",
                                   "--static-ipv6-address", ADDRESS6 + "/64", "--static-ipv6-gateway", GATEWAY6,
                                   "--static-ipv6-dns", DNS6], timeout=180)
    source_receipt.write_bytes(raw)
    receipt = json.loads(raw)
    assert (receipt["format"], receipt["wired_driver"], receipt["network_profile"]) == (
        8, "e1000e", "e1000e-static-dual-stack"), receipt

    stage = work / "install-source"
    stage.mkdir()
    verify = ("from pathlib import Path; import sys; from install_source import stage_image\n"
              "work, image, receipt = map(Path, sys.argv[1:4])\n"
              "_, facts = stage_image(work, image, receipt, sys.argv[4])\n"
              "assert facts['wired_driver'] == 'e1000e' and facts['network_profile'] == 'e1000e-static-dual-stack'\n")
    subprocess.run([sys.executable, "-B", "-c", verify, stage, image, source_receipt, sha(source_receipt)],
                   cwd=args.installer.resolve().parent, check=True, timeout=90)
    shutil.rmtree(stage)

    guest = dict(network_model="e1000e", ipv6_network=NETWORK, pace_commands=True)
    with boot(work, image, network_mac=MAC, **guest) as console:
        console.login("psios", operator)
        check_profile(console)
        node = call(console, "Node.Get", {})["reference"]["resource_id"]
        task = json.loads(console.run("printf '{}' | psiosctl submit system.status --key static-e1000e"))["task_id"]
        result = call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})
        assert result["lifecycle"] == "Succeeded"
        console.send("exit")
        console.login("root", admin)
        console.run("busybox ping -c 1 -W 3 192.0.2.2")
        console.run(f"busybox ping6 -c 1 -W 3 {GATEWAY6}")
        shutdown(console, image)

    original = sha(image)
    candidate = work / "candidate.raw"
    raw = subprocess.check_output([sys.executable, "-B", args.installer, candidate, *pinned,
                                   "--upgrade-from", image, "--previous-receipt", source_receipt], timeout=360)
    updated = json.loads(raw)
    assert updated["network_profile"] == receipt["network_profile"] and updated["wired_driver"] == "e1000e"
    assert sha(image) == original
    with boot(work, candidate, network_mac=MAC, **guest) as console:
        console.login("psios", operator)
        check_profile(console)
        assert call(console, "Node.Get", {})["reference"]["resource_id"] == node
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        console.send("exit")
        console.login("root", admin)
        shutdown(console, candidate)

    with boot(work, candidate, network_mac=OTHER, **guest) as console:
        console.login("psios", operator)
        wait_for(console, "grep -Fxq 'configured e1000e interface is absent' /run/psios-network/status")
        console.run("test ! -s /etc/resolv.conf && ! busybox ip -4 addr show dev eth0 | grep -q 'inet '")
        console.run("! busybox ip -6 addr show dev eth0 | grep -q 'scope global'")
        console.send("exit")
        console.login("root", admin)
        console.run("chmod 0666 /etc/psios/static-ipv6")
        shutdown(console, candidate)
    candidate_receipt = work / "candidate.json"
    candidate_receipt.write_bytes(raw)
    unsafe = work / "unsafe.raw"
    before = sha(candidate)
    refusal = subprocess.run([sys.executable, "-B", args.installer, unsafe, *pinned, "--upgrade-from", candidate,
                              "--previous-receipt", candidate_receipt], capture_output=True, text=True, timeout=360)
    assert refusal.returncode == 1 and "invalid persisted static IPv6 profile" in refusal.stderr, refusal.stderr
    assert not unsafe.exists() and sha(candidate) == before
    print("Verified e1000e static IPv4 and IPv6 traffic, clean upgrade, wrong-MAC isolation and unsafe-profile refusal.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-e1000e-static-") as directory:
        exercise(args, Path(directory))
