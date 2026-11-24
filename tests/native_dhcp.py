"""Boot an installed UEFI disk with real DHCP inside a private host network namespace."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, password, sha

MAC = "52:54:00:51:6f:20"
OTHER = "52:54:00:51:6f:21"


def wait_for(c, command):
    return c.run(f"for i in $(seq 1 100); do ( {command} ) && exit 0; sleep 0.1; done; exit 1")


def check_lease(c):
    try:
        wait_for(c, "busybox ip -4 addr show dev eth0 | grep -Fq '192.0.2.20/24' && "
                    "busybox ip -4 route show default | grep -Eq '^default via 192[.]0[.]2[.]2 dev eth0 +metric 30310 *$' && "
                    "grep -qx 'nameserver 192.0.2.3' /etc/resolv.conf")
    except AssertionError as failure:
        observed = c.run("busybox ip -4 addr show dev eth0; busybox ip -4 route show; cat /etc/resolv.conf; true")
        raise AssertionError(observed) from failure


def exercise(args, work):
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
    subprocess.run(build + [MAC], check=True, stdout=subprocess.DEVNULL, timeout=150)

    with boot(work, image, network_mac=MAC) as c:
        c.login("psios", user_secret)
        identity = json.loads(c.run("psiosctl whoami"))["authority_id"]
        check_lease(c)
        c.run("test \"$(cat /sys/class/net/eth0/address)\" = " + MAC)
        c.run("test \"$(cat /proc/sys/net/ipv6/conf/eth0/disable_ipv6)\" = 1")
        c.send("exit")
        c.login("root", root_secret)
        before = c.run("stat -Lc %i /etc/resolv.conf")
        c.run("kill -USR1 $(pidof udhcpc)")
        wait_for(c, f"test \"$(stat -Lc %i /etc/resolv.conf)\" != {before}")
        check_lease(c)
        c.run("kill -USR2 $(pidof udhcpc)")
        wait_for(c, "! busybox ip -4 addr show dev eth0 | grep -q 'inet ' && "
                    "test -z \"$(busybox ip -4 route show default)\" && test ! -s /etc/resolv.conf")
        c.run("kill -USR1 $(pidof udhcpc)")
        check_lease(c)
        c.send("poweroff")
        assert c.vm.wait(30) == 0
    check_clean_shutdown(image)

    with boot(work, image, network_mac=OTHER) as c:
        c.login("psios", user_secret)
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
        wait_for(c, f"test \"$(cat /sys/class/net/eth0/address 2>/dev/null)\" = {OTHER}")
        c.run("test ! -s /etc/resolv.conf && ! busybox ip -4 addr show dev eth0 | grep -q 'inet '")
        c.run("test -z \"$(busybox ip -4 route show default)\"")
        c.send("exit")
        c.login("root", root_secret)
        c.send("poweroff")
        assert c.vm.wait(30) == 0
    check_clean_shutdown(image)
    print("Verified DHCP address, scoped default route and DNS through renewal, release, rebind and wrong-MAC boot with the same local authority.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-dhcp-test-") as directory:
        exercise(args, Path(directory))
