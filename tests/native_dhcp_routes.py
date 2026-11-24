"""Serve actual DHCPv4 classless routes to an installed PsiOS on an isolated TAP."""
import argparse
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

from native_dhcp import MAC, prepare, wait_for
from native_disk import REPO, boot, call, check_clean_shutdown

TAP = "psios-test-tap"


def local_link():
    links = json.loads(subprocess.check_output(["ip", "-j", "link", "show"]))
    if os.geteuid() != 0 or [link["ifname"] for link in links] != ["lo"]:
        raise ValueError("run the test as root inside a fresh Linux network namespace")
    for args in (
        ["link", "set", "lo", "up"],
        ["tuntap", "add", "dev", TAP, "mode", "tap"],
        ["addr", "add", "192.0.2.2/24", "dev", TAP],
        ["addr", "add", "192.0.2.4/24", "dev", TAP],
        ["addr", "add", "203.0.113.1/24", "dev", TAP],
        ["addr", "add", "198.51.100.7/32", "dev", "lo"],
        ["link", "set", TAP, "up"],
    ):
        subprocess.run(["ip", *args], check=True)


@contextmanager
def server(work, name, routes, dns):
    pathname = work / f"{name}.log"
    command = [
        "dnsmasq", "--keep-in-foreground", "--conf-file=/dev/null", "--port=0", "--bind-interfaces",
        "--no-hosts", "--no-resolv", "--log-dhcp", "--log-facility=-", "--user=root", "--group=root",
        "--interface=" + TAP, "--dhcp-range=192.0.2.20,192.0.2.30,255.255.255.0,2m",
        f"--dhcp-host={MAC},192.0.2.20", "--dhcp-option-force=option:router,192.0.2.99",
        f"--dhcp-option-force=option:dns-server,{dns}", "--dhcp-leasefile=" + str(work / "leases"),
    ]
    if routes:
        command += ["--dhcp-option-force=option:classless-static-route," + routes]
    with pathname.open("w") as log, subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT) as service:
        try:
            deadline = time.monotonic() + 5
            while "IP range" not in pathname.read_text():
                assert service.poll() is None and time.monotonic() < deadline, pathname.read_text()
                time.sleep(0.05)
            yield pathname
        finally:
            if service.poll() is None:
                service.terminate()
                service.wait(5)


def routes(c):
    return c.run("busybox ip -4 route show dev eth0").splitlines()


def has(c, prefix, gateway=None, metric=30310):
    for line in routes(c):
        fields = line.split()
        if fields[0] == prefix and "metric" in fields and fields[fields.index("metric") + 1] == str(metric):
            via = fields[fields.index("via") + 1] if "via" in fields else None
            if via == gateway:
                return True
    return False


def change_lease(c, dns):
    c.run("kill -USR1 $(pidof udhcpc)")
    wait_for(c, f"grep -Fxq 'nameserver {dns}' /etc/resolv.conf")


def lookup(c, delegation, key, gateway):
    address = shlex.quote(json.dumps({"destination": "198.51.100.7"}))
    task = json.loads(c.run(f"printf '%s' {address} | psiosctl submit node.route_lookup "
                            f"--delegation {shlex.quote(delegation)} --key {key}"))
    result = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert result["lifecycle"] == "Succeeded", result
    reference, = result["result"]["artifact_refs"]
    report = call(c, "Artifact.Read", {"reference": reference})
    assert report["outcome"] == "Resolved" and report["route"]["gateway"] == gateway, report
    assert report["route"]["output_ifindex"] == int(c.run("cat /sys/class/net/eth0/ifindex")), report
    return reference, report


def exercise(args, work):
    local_link()
    image, user_secret, root_secret = prepare(args, work)
    initial = "198.51.100.0/24,192.0.2.2,203.0.113.5/25,0.0.0.0,0.0.0.0/0,192.0.2.2"
    with ExitStack() as guest:
        with server(work, "initial", initial, "192.0.2.2") as log:
            c = guest.enter_context(boot(work, image, network_mac=MAC, tap=TAP))
            c.login("psios", user_secret)
            identity = json.loads(c.run("psiosctl whoami"))["authority_id"]
            wait_for(c, "grep -Fxq 'nameserver 192.0.2.2' /etc/resolv.conf")
            assert has(c, "default", "192.0.2.2"), routes(c)
            assert has(c, "198.51.100.0/24", "192.0.2.2"), routes(c)
            assert has(c, "203.0.113.0/25"), routes(c)
            assert not any("192.0.2.99" in line for line in routes(c)), routes(c)
            grant = call(c, "Grant.Issue", {
                "principal_id": "local-owner", "tool_targets": [identity], "units": 2, "ttl_seconds": 120,
                "actions": ["node.route_lookup", "task.submit", "task.read"],
            })
            delegation = grant["delegation_ref"]["resource_id"]
            original, first = lookup(c, delegation, "initial-lease-route", "192.0.2.2")
            assert any("121" in line for line in log.read_text().splitlines() if "requested options:" in line), log.read_text()
            c.send("exit")
            c.login("root", root_secret)
            c.run("busybox ping -c 1 -W 2 198.51.100.7 >/dev/null && busybox ping -c 1 -W 2 203.0.113.1 >/dev/null")
            c.run("busybox ip -4 route add 198.51.100.0/24 dev eth0 proto 4 metric 41000")

        with server(work, "renew", "198.51.100.0/24,192.0.2.4", "192.0.2.4"):
            change_lease(c, "192.0.2.4")
            assert has(c, "198.51.100.0/24", "192.0.2.4"), routes(c)
            assert has(c, "198.51.100.0/24", metric=41000), routes(c)
            assert not any(line.startswith(("default ", "203.0.113.0/25 ")) for line in routes(c)), routes(c)
            c.send("exit")
            c.login("psios", user_secret)
            lookup(c, delegation, "renewed-lease-route", "192.0.2.4")
            assert call(c, "Artifact.Read", {"reference": original}) == first
            c.send("exit")
            c.login("root", root_secret)
            client = c.run("pidof udhcpc")
            resolver = c.run("stat -Lc %i /etc/resolv.conf")
            c.run(f"kill -KILL {client}")
            wait_for(c, f'test -n "$(pidof udhcpc)" && test "$(pidof udhcpc)" != {client} && '
                        f'test "$(stat -Lc %i /etc/resolv.conf 2>/dev/null)" != {resolver} && '
                        "grep -Fxq 'nameserver 192.0.2.4' /etc/resolv.conf")
            assert has(c, "198.51.100.0/24", "192.0.2.4"), routes(c)
            c.run("kill -USR2 $(pidof udhcpc)")
            wait_for(c, "! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && "
                        "test ! -s /etc/resolv.conf && grep -Fxq 'No active lease on eth0' /run/psios-network/status")
            assert not any("metric 30310" in line for line in routes(c)), routes(c)

        defective = "198.51.100.0/25,192.0.2.2,198.18.0.0/15,198.19.250.250"
        with server(work, "defective", defective, "192.0.2.2"):
            c.run("kill -USR1 $(pidof udhcpc)")
            wait_for(c, "grep -Fxq 'Could not apply the lease on eth0' /run/psios-network/status")
            c.run("! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && test ! -s /etc/resolv.conf")
            assert not any("metric 30310" in line for line in routes(c)), routes(c)
            c.run("kill -USR2 $(pidof udhcpc)")
            wait_for(c, "grep -Fxq 'No active lease on eth0' /run/psios-network/status")

        with server(work, "fallback", None, "192.0.2.2"):
            change_lease(c, "192.0.2.2")
            assert has(c, "default", "192.0.2.99"), routes(c)
            assert len([line for line in routes(c) if "metric 30310" in line]) == 1, routes(c)
            c.send("exit")
            c.login("psios", user_secret)
            assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
            c.send("exit")
            c.login("root", root_secret)
            c.send("poweroff")
            assert c.vm.wait(35) == 0
    check_clean_shutdown(image)
    print("PASS actual DHCP option 121, on-link and routed traffic, precedence, renewal, release, rollback and fallback")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-dhcp-routes-") as directory:
        exercise(args, Path(directory))
