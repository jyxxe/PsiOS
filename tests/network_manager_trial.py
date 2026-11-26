"""Verify real NetworkManager checkpoints inside private Linux namespaces.

Run with sudo unshare --net --mount --uts --propagation private \
    python3 -B tests/network_manager_trial.py
"""
from contextlib import ExitStack, contextmanager
from dataclasses import dataclass
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import socket
import subprocess
import sys
from tempfile import TemporaryDirectory
import time


NM = "org.freedesktop.NetworkManager"
NM_PATH = "/org/freedesktop/NetworkManager"
DBUS = "org.freedesktop.DBus"
DBUS_PATH = "/org/freedesktop/DBus"
DEVICE = "psios-nm0"
PEER = "psios-nmpeer0"
DEVICE_MAC = "02:50:53:00:00:01"
BUS_SOCKET = Path("/run/dbus/system_bus_socket")
BUS_ADDRESS = f"unix:path={BUS_SOCKET}"


@dataclass(frozen=True)
class Profile:
    name: str
    uuid: str
    local: str
    peer: str
    subnet: str

    def keyfile(self):
        return f"""[connection]
id={self.name}
uuid={self.uuid}
type=ethernet
interface-name={DEVICE}
autoconnect=false

[ethernet]

[ipv4]
method=manual
address1={self.local}/24
never-default=true
ignore-auto-dns=true

[ipv6]
method=disabled
"""


ORIGINAL = Profile("psios-original", "1aad7685-9a7d-49ad-8132-0c9c56cfb34e",
                   "192.0.2.10", "192.0.2.20", "192.0.2.0/24")
CANDIDATE = Profile("psios-candidate", "597da7b3-c8ba-4134-bfa7-57377f1ab670",
                    "198.51.100.10", "198.51.100.20", "198.51.100.0/24")


@dataclass(frozen=True)
class Checkpoint:
    path: str
    deadline: float


def command(*args, env=None):
    return subprocess.run(args, check=True, timeout=8, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env).stdout


def ip_json(*args):
    return json.loads(command("ip", "-j", *args))


def require_private_network():
    if (os.geteuid() != 0 or os.path.samefile("/proc/self/ns/net", "/proc/1/ns/net")
            or [link["ifname"] for link in ip_json("link", "show")] != ["lo"]):
        raise RuntimeError("run as root inside a fresh network namespace with only loopback")


def require_private_host_resources():
    require_private_network()
    for kind in ("mnt", "uts"):
        if os.path.samefile(f"/proc/self/ns/{kind}", f"/proc/1/ns/{kind}"):
            raise RuntimeError("unshare the mount and UTS namespaces before running this fixture")
    for mount in Path("/proc/self/mountinfo").read_text().splitlines():
        if any(field.startswith("shared:") for field in mount.split(" - ", 1)[0].split()[6:]):
            raise RuntimeError("the fixture requires private mount propagation")


@contextmanager
def child(args, **options):
    process = subprocess.Popen(args, **options)
    try:
        yield process
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(3)
        for stream in (process.stdin, process.stdout):
            if stream is not None:
                stream.close()


def peer_line(process):
    if not select.select([process.stdout], [], [], 8)[0]:
        raise TimeoutError("veth peer did not report readiness")
    return process.stdout.readline().decode().strip()


def peer():
    require_private_network()
    print("namespace ready", flush=True)
    if sys.stdin.readline() != "configure\n":
        raise RuntimeError("veth peer setup was not requested")
    command("ip", "link", "set", "lo", "up")
    command("ip", "link", "set", PEER, "up")
    for profile in (ORIGINAL, CANDIDATE):
        command("ip", "address", "add", f"{profile.peer}/24", "dev", PEER)
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("0.0.0.0", 0))
        listener.listen()
        print(listener.getsockname()[1], flush=True)
        while True:
            connection, (source, _) = listener.accept()
            with connection, connection.makefile("rb") as stream:
                connection.settimeout(3)
                payload = stream.readline(128)
                if payload:
                    destination = connection.getsockname()[0]
                    connection.sendall(f"{source} {destination} ".encode() + payload)


def until(description, read, expected, timeout=10):
    end = time.monotonic() + timeout
    while True:
        try:
            actual = read()
        except subprocess.CalledProcessError as error:
            actual = error.stderr.strip()
        if actual == expected:
            return
        if time.monotonic() >= end:
            raise AssertionError(f"{description}: expected {expected!r}, observed {actual!r}")
        time.sleep(0.15)


def scalar(fields, signature):
    assert len(fields) == 2 and fields[0] == signature, fields
    return fields[1]


def objects(fields):
    assert len(fields) >= 2 and fields[:2] == ["ao", str(len(fields) - 2)], fields
    return fields[2:]


def mount_private(stack, *args):
    command("mount", *args)
    stack.callback(command, "umount", str(args[-1]))


def remove_veth():
    try:
        socket.if_nametoindex(DEVICE)
    except OSError:
        return
    command("ip", "link", "delete", DEVICE)


class IsolatedNetworkManager:
    interface = DEVICE
    mac = DEVICE_MAC
    bus_address = BUS_ADDRESS
    original = ORIGINAL
    candidate = CANDIDATE

    def __init__(self, root):
        self.root = root
        self.environment = dict(os.environ, DBUS_SYSTEM_BUS_ADDRESS=self.bus_address)
        for key in ("DBUS_SESSION_BUS_ADDRESS", "NOTIFY_SOCKET", "LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"):
            self.environment.pop(key, None)

    def bus_command(self, *args):
        return command("busctl", f"--address={self.bus_address}", "--no-pager", "--auto-start=no",
                       "--allow-interactive-authorization=no", "--timeout=3s", *args)

    def busctl(self, *args):
        return shlex.split(self.bus_command(*args))

    def bus_property(self, object_path, interface, key):
        return self.busctl("get-property", self.manager_owner, object_path, interface, key)

    def manager_call(self, method, *args):
        return self.busctl("call", self.manager_owner, NM_PATH, NM, method, *args)

    def setup_peer(self, stack):
        command("ip", "link", "set", "lo", "up")
        process = stack.enter_context(child(["unshare", "--net", sys.executable, "-B",
                                              Path(__file__).resolve(), "--peer"],
                                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0))
        assert peer_line(process) == "namespace ready"
        command("ip", "link", "add", DEVICE, "address", DEVICE_MAC, "type", "veth", "peer", "name", PEER)
        stack.callback(remove_veth)
        command("ip", "link", "set", PEER, "netns", str(process.pid))
        command("ip", "link", "set", DEVICE, "up")
        process.stdin.write(b"configure\n")
        self.peer_port = int(peer_line(process))

    def setup_manager(self, stack):
        etc = self.root / "etc"
        etc.mkdir()
        connections = etc / "system-connections"
        connections.mkdir(mode=0o700)
        for profile in (ORIGINAL, CANDIDATE):
            config = connections / f"{profile.name}.nmconnection"
            config.touch(mode=0o600)
            config.write_text(profile.keyfile())
        local_conf = etc / "conf.d"
        local_conf.mkdir()
        vendor_conf = self.root / "empty-vendor-conf"
        vendor_conf.mkdir()
        config = etc / "NetworkManager.conf"
        config.write_text(f"""[main]
plugins=keyfile
no-auto-default=*
dns=none
rc-manager=unmanaged
hostname-mode=none
auth-polkit=false

[connectivity]
enabled=false

[keyfile]
unmanaged-devices=*;except:interface-name:{DEVICE}
""")
        state = self.root / "state"
        state.mkdir(mode=0o700)
        mount_private(stack, "-t", "tmpfs", "-o", "size=8m,nodev,nosuid,noexec", "tmpfs", "/run")
        mount_private(stack, "--bind", etc, "/etc/NetworkManager")
        mount_private(stack, "--bind", state, "/var/lib/NetworkManager")
        if Path("/dev/rfkill").exists():
            mount_private(stack, "--bind", "/dev/null", "/dev/rfkill")
        BUS_SOCKET.parent.mkdir()
        dbus_config = self.root / "dbus.conf"
        dbus_config.write_text(f"""<busconfig>
  <type>system</type>
  <auth>EXTERNAL</auth>
  <listen>{self.bus_address}</listen>
  <policy context="default">
    <deny user="*"/>
    <allow user="root"/>
  </policy>
  <policy user="root">
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
""")
        dbus = self.start_logged(stack, "dbus", ["dbus-daemon", "--nofork", f"--config-file={dbus_config}"])
        until("private D-Bus is listening", BUS_SOCKET.is_socket, True)
        self.bus_id = scalar(self.busctl("call", DBUS, DBUS_PATH, DBUS, "GetId"), "s")
        manager = self.start_logged(stack, "manager", [
            "NetworkManager", "--no-daemon", "--log-level=INFO", f"--config={config}",
            f"--config-dir={local_conf}", f"--system-config-dir={vendor_conf}",
            f"--state-file={state / 'NetworkManager.state'}",
            f"--intern-config={state / 'NetworkManager-intern.conf'}",
            "--pid-file=/run/dbus/psios-test-NetworkManager.pid",
        ])

        def manager_is_listening():
            assert dbus.poll() is None and manager.poll() is None
            self.manager_owner = scalar(self.busctl("call", DBUS, DBUS_PATH, DBUS, "GetNameOwner", "s", NM), "s")
            return self.manager_owner.startswith(":")

        until("private NetworkManager owns its name", manager_is_listening, True)
        pid = scalar(self.busctl("call", DBUS, DBUS_PATH, DBUS, "GetConnectionUnixProcessID", "s",
                                self.manager_owner), "u")
        assert int(pid) == manager.pid, (pid, manager.pid)

        def managed_veth():
            self.device = scalar(self.manager_call("GetDeviceByIpIface", "s", self.interface), "o")
            managed = scalar(self.bus_property(self.device, f"{NM}.Device", "Managed"), "b")
            state = scalar(self.bus_property(self.device, f"{NM}.Device", "State"), "u")
            return managed, state

        until("private NetworkManager can activate the spare veth", managed_veth, ("true", "30"))
        self.activate(ORIGINAL)
        self.assert_profile(ORIGINAL)

    def start_logged(self, stack, name, args):
        with (self.root / f"{name}.log").open("w") as log:
            return stack.enter_context(child(args, stdout=log, stderr=subprocess.STDOUT, env=self.environment))

    def profile_path(self, profile):
        return scalar(self.busctl("call", self.manager_owner, f"{NM_PATH}/Settings", f"{NM}.Settings",
                                   "GetConnectionByUuid", "s", profile.uuid), "o")

    def activate(self, profile):
        connection = self.profile_path(profile)
        return scalar(self.manager_call("ActivateConnection", "ooo", connection, self.device, "/"), "o")

    def settings(self, profile):
        return json.loads(self.bus_command("--json=short", "call", self.manager_owner, self.profile_path(profile),
                                           f"{NM}.Settings.Connection", "GetSettings"))

    def checkpoints(self):
        return objects(self.bus_property(NM_PATH, NM, "Checkpoints"))

    def checkpoint(self, seconds):
        assert seconds > 0 and not self.checkpoints()
        path = scalar(self.manager_call("CheckpointCreate", "aouu", "1", self.device, str(seconds), "0"), "o")
        assert self.checkpoints() == [path]
        assert objects(self.bus_property(path, f"{NM}.Checkpoint", "Devices")) == [self.device]
        assert scalar(self.bus_property(path, f"{NM}.Checkpoint", "RollbackTimeout"), "u") == str(seconds)
        created_ms = int(scalar(self.bus_property(path, f"{NM}.Checkpoint", "Created"), "x"))
        return Checkpoint(path, created_ms / 1000 + seconds)

    def confirm_checkpoint(self, checkpoint):
        assert self.checkpoints() == [checkpoint.path]
        assert self.manager_call("CheckpointDestroy", "o", checkpoint.path) == []
        assert self.checkpoints() == []

    def assert_profile(self, profile):
        device = f"{NM}.Device"

        def active_profile():
            active = scalar(self.bus_property(self.device, device, "ActiveConnection"), "o")
            if active != "/" and scalar(self.bus_property(active, f"{NM}.Connection.Active", "State"), "u") == "2":
                return scalar(self.bus_property(active, f"{NM}.Connection.Active", "Uuid"), "s")

        until("active wired profile", active_profile, profile.uuid)
        ip4 = scalar(self.bus_property(self.device, device, "Ip4Config"), "o")
        assert self.bus_property(ip4, f"{NM}.IP4Config", "NameserverData") == ["aa{sv}", "0"]
        link, = ip_json("-4", "address", "show", "dev", self.interface)
        addresses = [(address["local"], address["prefixlen"]) for address in link["addr_info"]]
        assert addresses == [(profile.local, 24)], link
        routes = ip_json("-4", "route", "show", "table", "main")
        assert [route["dst"] for route in routes if route.get("dev") == self.interface] == [profile.subnet], routes
        assert all(route.get("dst") != "default" for route in routes), routes
        assert not ip_json("-6", "route", "show", "default")
        with socket.create_connection((profile.peer, self.peer_port), timeout=3) as connection:
            with connection.makefile("rb") as stream:
                assert connection.getsockname()[0] == profile.local
                connection.sendall(b"probe\n")
                assert stream.readline(128) == f"{profile.local} {profile.peer} probe\n".encode()


@contextmanager
def isolated_manager():
    for dependency in ("ip", "unshare", "mount", "umount", "dbus-daemon", "busctl", "NetworkManager"):
        if not shutil.which(dependency):
            raise RuntimeError(f"missing native NetworkManager fixture dependency: {dependency}")
    require_private_host_resources()
    with TemporaryDirectory(prefix="psios-nm-", dir="/tmp") as directory:
        root = Path(directory)
        try:
            with ExitStack() as stack:
                fixture = IsolatedNetworkManager(root)
                fixture.setup_peer(stack)
                fixture.setup_manager(stack)
                yield fixture
        except Exception as error:
            if isinstance(error, subprocess.CalledProcessError) and error.stderr:
                print(error.stderr.strip(), file=sys.stderr)
            for log in sorted(root.glob("*.log")):
                with log.open("rb") as handle:
                    handle.seek(max(0, log.stat().st_size - 12000))
                    print(f"{log.name}:\n{handle.read().decode(errors='replace')}", file=sys.stderr)
            raise


def boottime():
    return time.clock_gettime(time.CLOCK_BOOTTIME)


def exercise():
    with isolated_manager() as manager:
        trial = manager.checkpoint(12)
        manager.activate(CANDIDATE)
        manager.assert_profile(CANDIDATE)
        assert boottime() < trial.deadline and manager.checkpoints() == [trial.path]
        until("NetworkManager expires the unconfirmed checkpoint", manager.checkpoints, [], timeout=16)
        manager.assert_profile(ORIGINAL)

        committed = manager.checkpoint(12)
        assert committed.path != trial.path
        manager.activate(CANDIDATE)
        manager.assert_profile(CANDIDATE)
        assert boottime() < committed.deadline
        manager.confirm_checkpoint(committed)
        while boottime() <= committed.deadline + 0.25:
            time.sleep(0.15)
        manager.assert_profile(CANDIDATE)
        print("PASS NetworkManager reverted an unconfirmed spare-link change and retained the confirmed change")


def print_settings():
    with isolated_manager() as manager:
        print(json.dumps({
            "device_type": int(scalar(manager.bus_property(manager.device, f"{NM}.Device", "DeviceType"), "u")),
            "original": manager.settings(ORIGINAL),
            "candidate": manager.settings(CANDIDATE),
        }, indent=2))


if __name__ == "__main__":
    if sys.argv[1:] == ["--peer"]:
        peer()
    elif sys.argv[1:] == ["--print-settings"]:
        print_settings()
    elif len(sys.argv) == 1:
        exercise()
    else:
        raise SystemExit(__doc__)
