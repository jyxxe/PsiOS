"""Exercise opt-in DNS router advertisements, expiration, and native update."""
import argparse
from contextlib import contextmanager
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

from native_dhcp import MAC, OTHER, wait_for
from native_disk import REPO, boot, call, check_clean_shutdown, password, sha, stage_guest_token
from native_slaac import TAP_ADDRESS, TAP_DNS, check_dns
from native_static_ipv4 import installer, poweroff


LINK_RESOLVER = "fe80::2"
RESOLVER_TOOL = "node.resolver"


@contextmanager
def router(work, tap, resolver):
    with tempfile.TemporaryDirectory(dir=work, prefix="ra-") as directory:
        root = Path(directory)
        settings = root / "radvd.conf"
        settings.write_text(f"""interface {tap} {{
    AdvSendAdvert on;
    MaxRtrAdvInterval 4;
    MinRtrAdvInterval 3;
    AdvDefaultLifetime 60;
    prefix 2001:db8:70::/64 {{ AdvOnLink on; AdvAutonomous on; }};
    RDNSS {resolver} {{ AdvRDNSSLifetime 12; }};
}};
""")
        process = subprocess.Popen(["radvd", "-n", "-m", "stderr", "-C", settings, "-p", root / "pid"])
        try:
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(10)


def advertised(console, resolver=TAP_DNS):
    wait_for(console, "grep -Fxq 'IPv6 router discovery with RDNSS on eth0' /run/psios-network/status")
    expected = TAP_ADDRESS.exploded.replace(":", "")
    wait_for(console, f"flags=$(awk '$1 == \"{expected}\" && $6 == \"eth0\" {{print $5}}' /proc/net/if_inet6); "
                      'test -n "$flags" && test "$((0x$flags & 72))" -eq 0')
    wait_for(console, f"grep -Fxq 'nameserver {resolver}' /etc/resolv.conf")
    assert "default via fe80::2 dev eth0" in console.run("busybox ip -6 route show default")
    console.run("! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && ! pidof udhcpc")
    assert "dhcpcd" in console.run("busybox ps -o user,comm | awk '$1 == \"dhcpcd\" && $2 ~ /^dhcpcd/ {print $1}'")
    check_dns(console)


def grant_resolver(console, node):
    agent = call(console, "Agent.Register", {"name": "native-rdnss-observer", "peer_uid": 1000})
    grant = call(console, "Grant.Issue", {
        "principal_id": agent["principal_id"], "actions": ["task.submit", "task.read", RESOLVER_TOOL],
        "tool_targets": [node], "units": 4, "ttl_seconds": 900,
    })
    return agent["token"], grant["delegation_ref"]["resource_id"]


def observe_resolver(console, token_file, delegation, key, node, expected):
    prepared = call(console, "Task.Prepare", {
        "tool": RESOLVER_TOOL, "key": key, "delegation_id": delegation, "input": {},
    }, token_file)
    receipt = call(console, "Task.Submit", prepared, token_file)
    task = call(console, "Task.Wait", {"id": receipt["task_id"], "timeout_ms": 5000}, token_file)
    assert task["lifecycle"] == "Succeeded", task
    reference, = task["result"]["artifact_refs"]
    snapshot = call(console, "Artifact.Read", {"reference": reference}, token_file)
    assert snapshot["path"] == "/etc/resolv.conf" and snapshot["invalid_nameserver_directives"] == 0, snapshot
    assert snapshot["boot_id"] == node["state"]["boot_id"], snapshot
    assert snapshot["network_namespace_inode"] == console.run("busybox stat -Lc '%i' /proc/self/ns/net"), snapshot
    assert snapshot["mount_namespace_inode"] == console.run("busybox stat -Lc '%i' /proc/self/ns/mnt"), snapshot
    assert snapshot["nameservers"] == expected, snapshot
    return reference, snapshot


def shutdown(console, root):
    console.send("exit")
    console.login("root", root)
    poweroff(console)


def exercise(args, work):
    for name, alias in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
        shutil.copyfile(args.ovmf_dir / name, work / alias)
    operator, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    image = work / "rdnss.raw"
    create = [*installer(args, image, args.archive), "--console-hash", operator_hash, "--root-console-hash", root_hash]
    invalid = subprocess.run([*create, "--slaac-mac", MAC, "--slaac-rdnss", "--slaac-dns", str(TAP_DNS)],
                             capture_output=True, text=True, timeout=10)
    assert invalid.returncode and "--slaac-rdnss requires" in invalid.stderr and not image.exists(), invalid.stderr

    prior = work / "rdnss.json"
    prior.write_bytes(subprocess.check_output([*create, "--slaac-mac", MAC, "--slaac-rdnss"], timeout=150))
    metadata = json.loads(prior.read_bytes())
    assert (metadata["format"], metadata["network_profile"]) == (11, "slaac-rdnss"), metadata
    assert sha(image) == metadata["initial_image_sha256"]
    with router(work, args.tap, TAP_DNS) as announcements, boot(work, image, network_mac=MAC, tap=args.tap) as console:
        console.login("psios", operator)
        advertised(console)
        node = call(console, "Node.Get", {})
        authority = node["reference"]["resource_id"]
        task = json.loads(console.run("printf '{}' | psiosctl submit system.status --key rdnss-persist"))["task_id"]
        result = call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"]
        if args.verify_core_resolver:
            secret, delegation = grant_resolver(console, authority)
            token = stage_guest_token(console, secret)
            initial_ref, initial = observe_resolver(console, token, delegation, "rdnss-initial", node,
                                                   [{"address": str(TAP_DNS)}])
        announcements.kill()
        announcements.wait(10)
        wait_for(console, "test -f /etc/resolv.conf && ! grep -q '^nameserver ' /etc/resolv.conf")
        assert "default via fe80::2 dev eth0" in console.run("busybox ip -6 route show default")
        if args.verify_core_resolver:
            observe_resolver(console, token, delegation, "rdnss-expired", node, [])
        with router(work, args.tap, LINK_RESOLVER):
            scoped = LINK_RESOLVER + "%eth0"
            advertised(console, scoped)
            if args.verify_core_resolver:
                observe_resolver(console, token, delegation, "rdnss-link-local", node,
                                 [{"address": LINK_RESOLVER, "zone": "eth0"}])
                console.run(f"rm -f -- {token}")
            console.send("exit")
            console.login("root", root)
            console.run("busybox ip link set dev eth0 down")
            wait_for(console, "test -f /etc/resolv.conf && ! grep -q '^nameserver ' /etc/resolv.conf")
            console.run("busybox ip link set dev eth0 up")
            advertised(console, scoped)
            poweroff(console)
    check_clean_shutdown(image)
    original = sha(image)

    stage = work / "staged"
    code, variables = work / "code.fd", work / "vars.fd"
    firmware = sha(variables)
    command = installer(args, stage, args.archive, args.installer.with_name("stage_update.py"))
    report = json.loads(subprocess.check_output([*command, "--upgrade-from", image, "--previous-receipt", prior,
        "--previous-receipt-sha256", sha(prior), "--ovmf-code", code, "--ovmf-code-sha256", sha(code),
        "--ovmf-vars", variables, "--ovmf-vars-sha256", firmware], timeout=480))
    assert report["status"] == "Staged" and sha(image) == original and sha(variables) == firmware
    candidate = stage / "candidate.raw"
    updated = json.loads((stage / "candidate-build.json").read_bytes())
    assert (updated["format"], updated["network_profile"]) == (11, "slaac-rdnss"), updated
    with router(work, args.tap, TAP_DNS), boot(work, candidate, network_mac=MAC, tap=args.tap) as console:
        console.login("psios", operator)
        advertised(console)
        current = call(console, "Node.Get", {})
        assert current["reference"]["resource_id"] == authority
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result
        if args.verify_core_resolver:
            token = stage_guest_token(console, secret)
            assert call(console, "Artifact.Read", {"reference": initial_ref}, token) == initial
            fresh_ref, fresh = observe_resolver(console, token, delegation, "rdnss-candidate", current,
                                                [{"address": str(TAP_DNS)}])
            assert fresh_ref != initial_ref and fresh["boot_id"] != initial["boot_id"], fresh
            console.run(f"rm -f -- {token}")
        shutdown(console, root)
    with router(work, args.tap, TAP_DNS), boot(work, candidate, network_mac=OTHER, tap=args.tap) as console:
        console.login("psios", operator)
        wait_for(console, "grep -Fxq 'configured virtio interface is absent' /run/psios-network/status")
        console.run("test ! -e /run/psios-network/resolv.conf && ! pidof dhcpcd")
        console.send("exit")
        console.login("root", root)
        console.run("sed -i 's/^dhcpcd:x:76:76:/dhcpcd:x:75:76:/' /etc/passwd")
        poweroff(console)
    with router(work, args.tap, TAP_DNS), boot(work, candidate, network_mac=MAC, tap=args.tap) as console:
        console.login("psios", operator)
        wait_for(console, "grep -Fxq 'router discovery service account is unavailable' /run/psios-network/status")
        console.run("test ! -e /run/psios-network/resolv.conf && ! pidof dhcpcd")
        shutdown(console, root)
    changed = sha(candidate)
    refused = work / "refused.raw"
    result = subprocess.run([*installer(args, refused, args.archive), "--upgrade-from", candidate,
                             "--previous-receipt", stage / "candidate-build.json"],
                            capture_output=True, text=True, timeout=120)
    assert result.returncode and "invalid persisted router discovery service account" in result.stderr, result.stderr
    assert not refused.exists() and sha(candidate) == changed
    assert sha(image) == original
    print("Verified native RDNSS, expiration, reacquisition, isolated service identity, staged update and the selected Virtio MAC.")
    if args.verify_core_resolver:
        print("Verified Agent resolver Tasks track live RDNSS and retain their exact artifacts through a native update.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--tap", default="psios6")
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--verify-core-resolver", action="store_true", help="require scoped node.resolver Tasks in the guest")
    args = parser.parse_args()
    assert len(args.tap) <= 15 and args.tap.isascii() and args.tap.isalnum(), args.tap
    with tempfile.TemporaryDirectory(prefix="psios-rdnss-") as root:
        exercise(args, Path(root))
