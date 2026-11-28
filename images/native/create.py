#!/usr/bin/env python3
"""Build a new PsiOS UEFI disk or prepare an offline upgrade of a stopped node."""
import argparse
import base64
import binascii
from copy import deepcopy
from dataclasses import asdict, dataclass, replace
import fcntl
from functools import partial
import gzip
import hashlib
import io
import ipaddress
import json
import os
from pathlib import Path
import re
import resource
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import uuid

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "rescue"))
import boot_media

MIB = 1024 * 1024
SECTOR_BYTES = 512
MAX_RAW_BYTES = (1 << 32) * SECTOR_BYTES
GPT_HEADER = struct.Struct("<8sIIIIQQQQ16sQIII")
CMDLINE = "console=ttyS0,115200 quiet panic=1 rdinit=/psios-init"
ESP = (1, 64, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
LINUX_PARTITION = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
MIN_STATE_MIB = 192
MIN_HOME_MIB = 64
MAX_IMAGE_MIB = 8192
EFI_PATH = "::/EFI/BOOT/BOOTX64.EFI"
STATIC_IPV4 = "virtio-static-ipv4"
STATIC_IPV6 = "virtio-static-ipv6"
STATIC_DUAL = "virtio-static-dual-stack"
VIRTIO_STATIC_MODES = {STATIC_IPV4: "static", STATIC_IPV6: "static6", STATIC_DUAL: "dual"}
E1000E_STATIC_MODES = {profile.replace("virtio-", "e1000e-", 1): mode for profile, mode in VIRTIO_STATIC_MODES.items()}
STATIC_MODES = VIRTIO_STATIC_MODES | E1000E_STATIC_MODES
LIMITED_SERVICE = "::respawn:/bin/sh /psios-core-cgroup serve"
TRIAL_BOOT_MODE = "IsolatedTrial"
TRIAL_RECEIPT_FORMAT = 7
E1000E_RECEIPT_FORMAT = 8
E1000E_MODULE = "psios-e1000e.ko"
DHCP_SERVICE = "::respawn:/bin/sh /psios-dhcp"
STATIC_SERVICE = "::once:/bin/sh /psios-dhcp "
SLAAC_RECEIPT_FORMAT = 10
SLAAC = "slaac"
SLAAC_SERVICE = STATIC_SERVICE + SLAAC
RDNSS = "slaac-rdnss"
RDNSS_RECEIPT_FORMAT = 11
RDNSS_PROGRAM = "sbin/dhcpcd"
RDNSS_PACKAGES = {"dhcpcd-10.5.2-r1": (RDNSS_PROGRAM,)}
RDNSS_ACCOUNT = {
    "passwd": ("0644", b"dhcpcd:x:76:76:IPv6 router discovery:/var/lib/dhcpcd:/bin/false"),
    "group": ("0644", b"dhcpcd:x:76:"),
    "shadow": ("0600", b"dhcpcd:!:0:0:99999:7:::"),
}
RA_SERVICES = {SLAAC: SLAAC_SERVICE, RDNSS: "::respawn:/bin/sh /psios-dhcp " + RDNSS}
SSH_RECEIPT_FORMAT = 9
SSH_PROFILE = "Ed25519Operator"
SSH_SERVICE = "::respawn:/bin/sh /psios-ssh"
SSH_PACKAGES = {
    "openssh-server-10.2_p1-r1": ("usr/sbin/sshd", "usr/lib/ssh/sshd-auth", "usr/lib/ssh/sshd-session"),
    "openssh-keygen-10.2_p1-r1": ("usr/bin/ssh-keygen",),
}
SSH_PROGRAMS = tuple(file for files in SSH_PACKAGES.values() for file in files)
TZDATA = "tzdata-2026f-r0"
TZDATA_SHA256 = "29b71c02c289e017ad43a96b00ff6bec39e6e07512cb67a647696320c54d0321"
ZONEINFO = "usr/share/zoneinfo"
ZONE_ARCHIVE = "psios-zoneinfo.tar"
TIMEZONE_COMMAND = "usr/bin/psios-timezone"


@dataclass(frozen=True)
class Layout:
    state_mib: int = MIN_STATE_MIB
    home_mib: int = MIN_HOME_MIB
    raw_disk_bytes: int | None = None

    def __post_init__(self):
        if (type(self.state_mib) is not int or type(self.home_mib) is not int
                or self.state_mib < MIN_STATE_MIB or self.home_mib < MIN_HOME_MIB
                or self.image_mib > MAX_IMAGE_MIB):
            raise ValueError(f"expected at least {MIN_STATE_MIB} MiB of state and {MIN_HOME_MIB} MiB of home "
                             f"within a {MAX_IMAGE_MIB} MiB raw image")
        size = self.raw_disk_bytes
        if size is not None and (type(size) is not int or not self.image_mib * MIB < size <= MAX_RAW_BYTES
                                 or size % SECTOR_BYTES):
            raise ValueError("larger raw capacity must use whole 512-byte sectors and be at most 2 TiB")

    @property
    def image_mib(self):
        return ESP[0] + ESP[1] + self.state_mib + self.home_mib + 2

    @property
    def disk_bytes(self):
        return self.raw_disk_bytes or self.image_mib * MIB

    def for_source(self, size):
        if size is None or size == self.disk_bytes:
            return self
        if self.raw_disk_bytes is not None:
            raise ValueError("source capacity does not match the previously recorded raw disk size")
        return replace(self, raw_disk_bytes=size)

    @property
    def partitions(self):
        state_start = ESP[0] + ESP[1]
        return (ESP, (state_start, self.state_mib, LINUX_PARTITION),
                (state_start + self.state_mib, self.home_mib, LINUX_PARTITION))

    def receipt(self):
        return {"state_mib": self.state_mib, "home_mib": self.home_mib, "image_mib": self.image_mib}

    @classmethod
    def from_receipt(cls, receipt):
        geometry = receipt.get("geometry_mib")
        if not isinstance(geometry, dict) or set(geometry) != {"state_mib", "home_mib", "image_mib"}:
            raise ValueError("invalid build receipt geometry")
        size = receipt.get("raw_disk_bytes")
        if "raw_disk_bytes" in receipt and type(size) is not int:
            raise ValueError("invalid build receipt raw capacity")
        layout = cls(geometry["state_mib"], geometry["home_mib"], size)
        if type(geometry["image_mib"]) is not int or geometry != layout.receipt():
            raise ValueError("invalid build receipt image size")
        return layout


@dataclass(frozen=True)
class CoreLimits:
    cpu_millicores: int
    memory_mib: int
    pids_max: int

    def __post_init__(self):
        # The 100 ms CFS period requires a quota of at least 1 ms.
        if type(self.cpu_millicores) is not int or not 10 <= self.cpu_millicores < (1 << 31):
            raise ValueError("Core CPU quota must be at least 10 millicores and fit in a signed 32-bit integer")
        if type(self.memory_mib) is not int or not 1 <= self.memory_mib < (1 << 31):
            raise ValueError("Core memory limit must be a positive signed 32-bit integer in MiB")
        if type(self.pids_max) is not int or not 1 <= self.pids_max < (1 << 31):
            raise ValueError("Core PID limit must be a positive signed 32-bit integer")

    @classmethod
    def from_receipt(cls, limits):
        if not isinstance(limits, dict) or set(limits) != {"cpu_millicores", "memory_mib", "pids_max"}:
            raise ValueError("previous build receipt must contain all Core resource limits")
        return cls(**limits)

    def kernel_values(self):
        return f"{self.cpu_millicores * 100} {self.memory_mib * MIB} {self.pids_max}\n"


@dataclass(frozen=True)
class TrialInput:
    disk_uuid: uuid.UUID
    configuration_sha256: str
    secure_boot_required: bool = False


@dataclass(frozen=True)
class PreparedUpdate:
    candidate_disk: Path
    trial_disk: Path
    candidate_receipt: dict
    trial_receipt: dict
    core: Path
    configuration: bytes
    normalized_source_sha256: str
    kernel_iso: Path
    max_copy_mib: int
    boot_tools: boot_media.BootTools


def sha256(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def install_core(work, archive, digest):
    package = work / "core.tar.gz"
    boot_media.pinned_copy(archive, package, digest, 64 * MIB)
    members = {"psiosd": "bin/psiosd", "psiosctl": "bin/psiosctl", "source-commit": "psios-source"}
    required = set(members)
    members["psios-mcp"] = "bin/psios-mcp"
    found = set()
    total = 0
    with tarfile.open(package, "r|gz") as bundle:
        for index, entry in enumerate(bundle):
            total += entry.size
            if index >= 128 or total > 128 * MIB or entry.name in found:
                raise ValueError("invalid Core archive layout")
            if entry.name not in members:
                continue
            if not entry.isfile() or not 0 < entry.size <= 64 * MIB:
                raise ValueError("invalid Core archive member")
            found.add(entry.name)
            path = work / members[entry.name]
            with path.open("xb") as dst:
                shutil.copyfileobj(bundle.extractfile(entry), dst)
            path.chmod(0o755 if entry.name != "source-commit" else 0o644)
    if not required.issubset(found) or not re.fullmatch(r"[0-9a-f]{40}\n", (work / "psios-source").read_text()):
        raise ValueError("incomplete Core archive")


def password_hash(path):
    with boot_media.regular_file(path) as f:
        if os.fstat(f.fileno()).st_mode & 0o077:
            raise ValueError("console hash file must be private")
        password = f.read(257)
    if not re.fullmatch(rb"\$6\$[./A-Za-z0-9]{1,16}\$[./A-Za-z0-9]{86}\n?", password):
        raise ValueError("expected one SHA-512 crypt password hash")
    return password.strip()


def ssh_public_key(raw):
    line = raw.removesuffix(b"\n")
    words = line.split(None, 2)
    if (len(raw) > 512 or any(byte != 9 and not 32 <= byte <= 126 for byte in line)
            or len(words) < 2 or words[0] != b"ssh-ed25519"):
        raise ValueError("expected one OpenSSH Ed25519 public key")
    try:
        value = base64.b64decode(words[1], validate=True)
    except binascii.Error as error:
        raise ValueError("invalid OpenSSH Ed25519 public key encoding") from error
    header = struct.pack(">I", len(words[0])) + words[0] + struct.pack(">I", 32)
    if len(value) != len(header) + 32 or not value.startswith(header) or base64.b64encode(value) != words[1]:
        raise ValueError("invalid OpenSSH Ed25519 public key data")
    return b"ssh-ed25519 " + words[1] + b"\n"


def unicast_mac(value):
    value = value.lower()
    if not re.fullmatch(r"[0-9a-f][02468ace](?::[0-9a-f]{2}){5}", value) or value == "00:00:00:00:00:00":
        raise argparse.ArgumentTypeError("expected a nonzero unicast MAC address")
    return value


def static_profile(mac, address, gateway, dns):
    subnet = address.network

    def host(ip):
        return (not (ip.is_multicast or ip.is_unspecified or ip.is_loopback or int(ip) == 0xffffffff)
                and (subnet.prefixlen >= 31 or ip not in (subnet.network_address, subnet.broadcast_address)))

    if not host(address.ip):
        raise ValueError("static address must identify an IPv4 host on the selected subnet")
    if gateway is not None and (gateway not in subnet or gateway == address.ip or not host(gateway)):
        raise ValueError("static gateway must be a different host on the selected subnet")
    if len(dns) > 3 or any(ip.is_multicast or ip.is_unspecified or int(ip) == 0xffffffff for ip in dns):
        raise ValueError("static DNS accepts at most three unicast IPv4 addresses")
    return "\n".join([mac, str(address), str(gateway) if gateway else "-", *map(str, dns)]) + "\n"


def ipv6_unicast(ip):
    return ip.scope_id is None and ip.ipv4_mapped is None and not (ip.is_unspecified or ip.is_multicast)


def ipv6_nameservers(dns):
    return len(dns) <= 3 and all(ipv6_unicast(ip) and not ip.is_link_local for ip in dns)


def static_ipv6_profile(mac, address, gateway, dns):
    subnet = address.network

    def on_link_host(ip):
        return (ip in subnet and not (ip.is_loopback or ip.is_link_local)
                and (subnet.prefixlen >= 127 or ip != subnet.network_address))

    if not (address.scope_id is None and subnet.prefixlen > 0 and ipv6_unicast(address.ip) and on_link_host(address.ip)):
        raise ValueError("static IPv6 address must be an unscoped host address with a nonzero prefix")
    if gateway is not None and (not ipv6_unicast(gateway) or gateway == address.ip
                                or not (gateway.is_link_local or on_link_host(gateway))):
        raise ValueError("static IPv6 gateway must be a different on-link or link-local address")
    if not ipv6_nameservers(dns):
        raise ValueError("static DNS accepts at most three unicast IPv6 addresses without a zone")
    host = f"{address.ip.exploded}/{subnet.prefixlen}"
    return "\n".join([mac, host, gateway.exploded if gateway else "-", *(ip.exploded for ip in dns)]) + "\n"


def slaac_profile(mac, dns):
    if not ipv6_nameservers(dns):
        raise ValueError("SLAAC DNS accepts at most three unicast IPv6 addresses without a zone")
    return "\n".join([mac, *(ip.exploded for ip in dns)]) + "\n"


def static_config(args, parser):
    options4 = args.static_address is not None or args.static_gateway is not None or args.static_dns
    options6 = args.static_ipv6_address is not None or args.static_ipv6_gateway is not None or args.static_ipv6_dns
    if args.static_mac is None:
        if options4 or options6:
            parser.error("static network options require --static-mac")
        return None, None
    if not (options4 or options6):
        parser.error("--static-mac requires a static address")
    if options4 and args.static_address is None:
        parser.error("IPv4 gateway or DNS requires --static-address")
    if options6 and args.static_ipv6_address is None:
        parser.error("IPv6 gateway or DNS requires --static-ipv6-address")
    if len(args.static_dns) + len(args.static_ipv6_dns) > 3:
        parser.error("static networking accepts at most three DNS servers in total")
    try:
        v4 = (static_profile(args.static_mac, args.static_address, args.static_gateway, args.static_dns)
              if options4 else None)
        v6 = (static_ipv6_profile(args.static_mac, args.static_ipv6_address, args.static_ipv6_gateway, args.static_ipv6_dns)
              if options6 else None)
        return v4, v6
    except ValueError as e:
        parser.error(str(e))


def validate_persisted_static(work, state, version):
    path = f"/etc/psios/static-ipv{version}"
    try:
        previous = read_root_file(work, state, path, 128 if version == 4 else 256, "0600").decode("ascii")
        mac, address, gateway, *dns = previous.splitlines()
        interface, ip, profile = ((ipaddress.IPv4Interface, ipaddress.IPv4Address, static_profile) if version == 4 else
                                  (ipaddress.IPv6Interface, ipaddress.IPv6Address, static_ipv6_profile))
        normalized = profile(unicast_mac(mac), interface(address), None if gateway == "-" else ip(gateway), list(map(ip, dns)))
        if previous != normalized:
            raise ValueError("expected canonical saved network settings")
        return mac, len(dns)
    except (OSError, ValueError, argparse.ArgumentTypeError, subprocess.SubprocessError) as e:
        raise ValueError(f"invalid persisted static IPv{version} profile") from e


def validate_persisted_e1000e(work, state):
    raw = read_root_file(work, state, "/etc/psios/dhcp-mac", 18, "0600")
    try:
        mac = unicast_mac(raw.decode("ascii").removesuffix("\n"))
    except (UnicodeError, argparse.ArgumentTypeError) as error:
        raise ValueError("invalid persisted e1000e DHCP MAC") from error
    inittab = read_root_file(work, state, "/etc/inittab", 16 * 1024, "0(?:600|644)")
    if raw != (mac + "\n").encode("ascii") or inittab.splitlines().count(DHCP_SERVICE.encode()) != 1:
        raise ValueError("invalid persisted e1000e DHCP configuration or launcher")


def validate_persisted_slaac(work, state, mode):
    try:
        profile = read_root_file(work, state, "/etc/psios/slaac", 160, "0600").decode("ascii")
        mac, *dns = profile.splitlines()
        if ((mode == RDNSS and dns)
                or profile != slaac_profile(unicast_mac(mac), [ipaddress.IPv6Address(ip) for ip in dns])):
            raise ValueError("expected canonical saved IPv6 discovery settings")
    except (OSError, ValueError, argparse.ArgumentTypeError, subprocess.SubprocessError) as error:
        raise ValueError("invalid persisted SLAAC profile") from error
    inittab = read_root_file(work, state, "/etc/inittab", 16 * 1024, "0(?:600|644)")
    if inittab.splitlines().count(RA_SERVICES[mode].encode()) != 1:
        raise ValueError("invalid persisted SLAAC service launcher")
    if mode == RDNSS:
        for name, (permissions, expected) in RDNSS_ACCOUNT.items():
            accounts = read_root_file(work, state, "/etc/" + name, 64 * 1024, permissions)
            if [line for line in accounts.splitlines() if line.startswith(b"dhcpcd:")] != [expected]:
                raise ValueError("invalid persisted router discovery service account")


def validate_persisted_network(work, state, receipt):
    profile = receipt.get("network_profile")
    if profile in RA_SERVICES:
        validate_persisted_slaac(work, state, profile)
        return
    mode = STATIC_MODES.get(profile)
    if mode in ("static", "dual"):
        mac4, dns4 = validate_persisted_static(work, state, 4)
    if mode in ("static6", "dual"):
        mac6, dns6 = validate_persisted_static(work, state, 6)
    if mode == "dual" and (mac4 != mac6 or dns4 + dns6 > 3):
        raise ValueError("inconsistent persisted dual-stack profile")
    if "wired_driver" not in receipt:
        return
    if mode is None:
        validate_persisted_e1000e(work, state)
        return
    inittab = read_root_file(work, state, "/etc/inittab", 16 * 1024, "0(?:600|644)")
    if inittab.splitlines().count((STATIC_SERVICE + mode).encode()) != 1:
        raise ValueError("invalid persisted e1000e static launcher")


def validate_persisted_ssh(work, state):
    key = read_root_file(work, state, "/etc/psios/ssh-authorized-key", 512, "0644")
    inittab = read_root_file(work, state, "/etc/inittab", 16 * 1024, "0(?:600|644)")
    if ssh_public_key(key) != key or inittab.splitlines().count(SSH_SERVICE.encode()) != 1:
        raise ValueError("invalid persisted SSH console key or launcher")


def host_config(seed, args):
    (seed / "etc/psios").mkdir(parents=True)
    operator = password_hash(args.console_hash)
    root = password_hash(args.root_console_hash) if args.root_console_hash else b"!"
    (seed / "etc/passwd").write_text("root:x:0:0:root:/root:/bin/sh\npsios:x:1000:1000:PsiOS operator:/home/psios:/bin/sh\n")
    (seed / "etc/group").write_text("root:x:0:\npsios:x:1000:\n")
    (seed / "etc/shadow").write_bytes(b"root:" + root + b":0:0:99999:7:::\npsios:" + operator + b":0:0:99999:7:::\n")
    (seed / "etc/shadow").chmod(0o600)
    (seed / "etc/os-release").write_text('NAME=PsiOS\nID=psios\nPRETTY_NAME="PsiOS amd64 development image"\n')
    if args.hostname:
        (seed / "etc/hostname").write_text(args.hostname + "\n")
    if args.timezone:
        (seed / "etc/localtime").symlink_to("../" + ZONEINFO + "/" + args.timezone)
    (seed / "etc/securetty").write_text("ttyS0\n")
    (seed / "etc/issue").write_text("PsiOS amd64 development console\n")
    (seed / "etc/psios/core.json").write_text(json.dumps({
        "state_dir": "/var/lib/psios", "witness_dir": "/var/lib/psios.witness", "socket": "/run/psios/api.sock"}))
    daemon = LIMITED_SERVICE if args.core_limits else "::respawn:/bin/su -s /bin/sh psios -c 'exec /bin/psiosd serve'"
    inittab = (f"{daemon}\nttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100\n"
               "::shutdown:/bin/sh /psios-shutdown\n")
    if args.network_profile in RA_SERVICES:
        (seed / "etc/psios/slaac").write_text(args.slaac_network)
        inittab += RA_SERVICES[args.network_profile] + "\n"
    elif args.network_profile:
        for name, config in (("static-ipv4", args.static_network), ("static-ipv6", args.static_ipv6_network)):
            if config:
                (seed / "etc/psios" / name).write_text(config)
        inittab += STATIC_SERVICE + STATIC_MODES[args.network_profile] + "\n"
    elif args.dhcp_mac:
        (seed / "etc/psios/dhcp-mac").write_text(args.dhcp_mac + "\n")
        inittab += DHCP_SERVICE + "\n"
    if args.dhcp_mac or args.network_profile:
        (seed / "etc/resolv.conf").symlink_to("/run/psios-network/resolv.conf")
    if args.ssh_authorized_key:
        with (seed / "etc/passwd").open("a") as accounts:
            accounts.write("sshd:x:74:74:OpenSSH privilege separation:/var/empty:/bin/false\n")
        with (seed / "etc/group").open("a") as groups:
            groups.write("sshd:x:74:\n")
        with (seed / "etc/shadow").open("ab") as shadows:
            shadows.write(b"sshd:!:0:0:99999:7:::\n")
        (seed / "etc/psios/ssh-authorized-key").write_bytes(args.ssh_authorized_key)
        inittab += SSH_SERVICE + "\n"
    if args.network_profile == RDNSS:
        for name, (_, account) in RDNSS_ACCOUNT.items():
            with (seed / "etc" / name).open("ab") as accounts:
                accounts.write(account + b"\n")
    (seed / "etc/inittab").write_text(inittab)
    # Target permissions are independent of the builder's umask.
    for path in (seed / "etc").rglob("*"):
        if not path.is_symlink():
            path.chmod(0o755 if path.is_dir() else (0o600 if path.name in
                       ("shadow", "dhcp-mac", "static-ipv4", "static-ipv6", "slaac") else 0o644))
    (seed / "etc").chmod(0o755)


def core_programs(work):
    programs = ["bin/psiosd", "bin/psiosctl"]
    if (work / "bin/psios-mcp").is_file():
        programs.append("bin/psios-mcp")
    return programs


def prepare_programs(work, packages, *, tools=None):
    for package, files in packages.items():
        archive = work / (package + ".apk")
        with archive.open("xb") as output:
            command = boot_media.tool_command(
                ["isoinfo", "-R", "-i", work / "base.iso", "-x", f"/apks/x86_64/{package}.apk"], tools)
            subprocess.run(command,
                           stdout=output, check=True, timeout=30, preexec_fn=file_limit(4 * MIB))
        remaining = set(files)
        with tarfile.open(archive, "r:gz", ignore_zeros=True) as bundle:
            for index, entry in enumerate(bundle):
                if index >= 128 or entry.size > 4 * MIB:
                    raise ValueError(f"{package} package member limit exceeded")
                if entry.name not in files:
                    continue
                if entry.name not in remaining or not entry.isfile() or not 0 < entry.size <= 2 * MIB:
                    raise ValueError(f"invalid native program member in {package}")
                path = work / entry.name
                path.parent.mkdir(parents=True, exist_ok=True)
                with path.open("xb") as destination:
                    shutil.copyfileobj(bundle.extractfile(entry), destination)
                remaining.remove(entry.name)
        if remaining:
            raise ValueError(f"{package} package is missing required programs")
        archive.unlink()


def timezone_name(value):
    return len(value) <= 96 and re.fullmatch(r"[A-Za-z0-9_+-]+(?:/[A-Za-z0-9_+-]+)*", value)


def prepare_timezones(work, *, tools=None):
    package = work / (TZDATA + ".apk")
    with package.open("xb") as output:
        command = boot_media.tool_command(
            ["isoinfo", "-R", "-i", work / "base.iso", "-x", f"/apks/x86_64/{package.name}"], tools)
        subprocess.run(command,
                       stdout=output, check=True, timeout=30, preexec_fn=file_limit(MIB))
    if sha256(package) != TZDATA_SHA256:
        raise ValueError("timezone package does not match the pinned Linux media")
    prefix = ZONEINFO + "/"
    zones = set()
    with tarfile.open(package, "r:gz", ignore_zeros=True) as source, \
            tarfile.open(work / ZONE_ARCHIVE, "x", format=tarfile.USTAR_FORMAT) as archive:
        for member in sorted(source.getmembers(), key=lambda entry: entry.name):
            if member.isdir() or not member.name.startswith(prefix):
                continue
            if member.islnk() and not member.linkname.startswith(prefix):
                raise ValueError("invalid timezone package member link")
            if not (member.isfile() or member.islnk()):
                raise ValueError("unsupported timezone package member type")
            with source.extractfile(member) as zone:
                data = zone.read(16 * 1024 + 1)
            if not data.startswith(b"TZif"):
                continue  # Skip zone tables and other non-binary metadata.
            name = member.name.removeprefix(prefix)
            if not timezone_name(name) or name in zones or len(data) > 16 * 1024:
                raise ValueError("invalid timezone package member")
            info = tarfile.TarInfo(name)
            info.mode = 0o444
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
            zones.add(name)
    package.unlink()
    if "UTC" not in zones:
        raise ValueError("timezone package is missing UTC data")
    command = work / TIMEZONE_COMMAND
    command.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(HERE / "timezone", command)
    return zones


def prepare_boot_inputs(work, args, *, wired_driver=None, ssh_enabled=False, network_profile=None, tools=None):
    base = boot_media.prepare_kernel(work, args.kernel_iso, tools=tools)
    if wired_driver == "e1000e":
        if shutil.which("unsquashfs") is None:
            raise ValueError("building the e1000e profile requires unsquashfs")
        modloop = work / "modloop"
        with modloop.open("xb") as out:
            command = boot_media.tool_command(["isoinfo", "-R", "-i", work / "base.iso", "-x", "/boot/modloop-virt"], tools)
            subprocess.run(command,
                           stdout=out, check=True, timeout=30, preexec_fn=file_limit(base["size_bytes"]))
        expected = base["e1000e_module"]
        module = work / E1000E_MODULE
        with module.open("xb") as out:
            command = boot_media.tool_command(["unsquashfs", "-cat", modloop, expected["member"]], tools)
            subprocess.run(command, stdout=out,
                           check=True, timeout=30, preexec_fn=file_limit(MIB))
        if sha256(module) != expected["sha256"]:
            raise ValueError("e1000e module does not match the pinned Linux media")
        modloop.unlink()
    (work / "bin").mkdir()
    install_core(work, args.core_archive, args.core_sha256)
    zones = prepare_timezones(work, tools=tools)
    if getattr(args, "timezone", None) and args.timezone not in zones:
        raise ValueError("unknown time zone in the pinned IANA data")
    if ssh_enabled:
        prepare_programs(work, SSH_PACKAGES, tools=tools)
    if network_profile == RDNSS:
        prepare_programs(work, RDNSS_PACKAGES, tools=tools)
    return base


def assemble_ramfs(work, args, base, instance, home_id, layout, signing, network_profile, *, core_limits, trial,
                   require_secure_boot=False, wired_driver=None, ssh_enabled=False, tools=None):
    (work / "psios-disk-uuid").write_text(str(instance) + "\n")
    (work / "psios-home-uuid").write_text(str(home_id) + "\n")
    sectors = [str(mib * 2048) for start, size, _ in layout.partitions[1:] for mib in (start, size)]
    (work / "psios-storage-layout").write_text(" ".join(sectors) + "\n")
    boot_scripts = ["init", "shutdown", "dhcp", "dhcp-event", "ipv4", "ipv6", "static", "storage", "enroll"]
    files = [*core_programs(work), "psios-source", "psios-disk-uuid", "psios-home-uuid", "psios-storage-layout",
             ZONE_ARCHIVE, TIMEZONE_COMMAND]
    if network_profile == RDNSS:
        boot_scripts.extend(("rdnss-hook", "rdnss.conf", "dhcpcd.LICENSE"))
        files.append(RDNSS_PROGRAM)
    if wired_driver:
        files.append(E1000E_MODULE)
    if ssh_enabled:
        boot_scripts.append("ssh")
        files.extend(SSH_PROGRAMS)
    if core_limits:
        boot_scripts.append("core-cgroup")
        (work / "psios-core-limits").write_text(core_limits.kernel_values())
        files.append("psios-core-limits")
    if trial:
        (work / "psios-trial-config.sha256").write_text(f"{trial.configuration_sha256}  /psios-trial-core.json\n")
        (work / "psios-trial-disk-id").write_text(trial.disk_uuid.bytes_le.hex() + "\n")
        files.extend(["psios-trial-config.sha256", "psios-trial-disk-id"])
        if trial.secure_boot_required:
            (work / "psios-trial-secure-boot-required").write_text("required\n")
            files.append("psios-trial-secure-boot-required")
    if require_secure_boot:
        (work / "psios-secure-boot-required").write_text("required\n")
        files.append("psios-secure-boot-required")
    for script in boot_scripts:
        shutil.copyfile(HERE / script, work / f"psios-{script}")
    files.extend(f"psios-{script}" for script in boot_scripts)
    payload = {"/" + name: sha256(work / name) for name in files}
    inputs = {
        "format": 1, "architecture": "x86_64", "firmware": "UEFI",
        "core_source_commit": (work / "psios-source").read_text().strip(), "core_archive_sha256": args.core_sha256,
        "kernel_input": {name: base[name] for name in ("release", "sha256", "kernel_sha256", "initramfs_sha256")},
        "geometry_mib": layout.receipt(),
        "required_filesystems": {"state": str(instance), "home": str(home_id)},
        "payload_sha256": payload,
        "timezone_data": {"package": TZDATA, "package_sha256": TZDATA_SHA256},
    }
    if layout.raw_disk_bytes is not None:
        inputs["raw_disk_bytes"] = layout.raw_disk_bytes
    if signing:
        inputs["secure_boot"] = signing.receipt()
    if network_profile:
        inputs["network_profile"] = network_profile
    if wired_driver:
        inputs["wired_driver"] = wired_driver
    if ssh_enabled:
        inputs["ssh_console"] = SSH_PROFILE
    if core_limits:
        inputs["core_limits"] = asdict(core_limits)
    if trial:
        inputs.update(boot_mode=TRIAL_BOOT_MODE, trial_disk_uuid=str(trial.disk_uuid))
    if require_secure_boot or (trial and trial.secure_boot_required):
        inputs["secure_boot_enforcement_required"] = True
    manifest = work / "psios-deployment.json"
    manifest.write_text(json.dumps(inputs, sort_keys=True, separators=(",", ":")) + "\n")
    # BusyBox can verify the exact extracted bytes without a JSON interpreter.
    checks = {**payload, "/psios-deployment.json": sha256(manifest)}
    (work / "psios-payload.sha256").write_text("".join(f"{digest}  {name}\n" for name, digest in checks.items()))
    files.extend(["psios-deployment.json", "psios-payload.sha256"])
    for name in files:
        executable = name.startswith("bin/") or name in (
            "psios-init", "psios-dhcp-event", "psios-rdnss-hook", RDNSS_PROGRAM, TIMEZONE_COMMAND, *SSH_PROGRAMS)
        (work / name).chmod(0o755 if executable else 0o644)
    if ssh_enabled:
        ssh_directory = "usr/lib/ssh"
        (work / ssh_directory).chmod(0o755)
        files.insert(0, ssh_directory)
    # GNU cpio's reproducible mode still includes each member's modification time.
    for name in files:
        os.utime(work / name, (boot_media.EPOCH, boot_media.EPOCH))
    append_deployment_payload(work, files, tools=tools)


def append_deployment_payload(work, files, *, tools=None):
    command = boot_media.tool_command(["cpio", "--quiet", "-o", "-H", "newc", "--reproducible", "--owner=0:0"],
                                      tools, work)
    # Supply the small name list separately so cpio's potentially large output is streamed.
    with tempfile.TemporaryFile(dir=work) as names, (work / "initrd").open("ab") as out:
        names.write(("\n".join(files) + "\n").encode())
        names.seek(0)
        out.write(b"\0" * (-out.tell() % 4))
        with subprocess.Popen(command, cwd=work, stdin=names, stdout=subprocess.PIPE) as archive:
            with gzip.GzipFile(filename="", mode="wb", fileobj=out, mtime=0, compresslevel=6) as member:
                shutil.copyfileobj(archive.stdout, member, length=MIB)
            if status := archive.wait():
                raise subprocess.CalledProcessError(status, command)


def ramfs(work, args, instance, home_id, layout=Layout(), signing=None, network_profile=None, *, core_limits=None,
          trial=None, wired_driver=None, ssh_enabled=False, tools=None):
    base = prepare_boot_inputs(work, args, wired_driver=wired_driver, ssh_enabled=ssh_enabled,
                               network_profile=network_profile, tools=tools)
    assemble_ramfs(work, args, base, instance, home_id, layout, signing, network_profile,
                   core_limits=core_limits, trial=trial, wired_driver=wired_driver, ssh_enabled=ssh_enabled, tools=tools)


def seeded_ext4(work, name, label, identity, size_mib, seed, owners):
    image = work / name
    with image.open("xb") as f:
        f.truncate(size_mib * MIB)
    boot_media.run(work, ["mkfs.ext4", "-q", "-F", "-U", str(identity), "-L", label,
                         "-E", "root_owner=0:0", "-d", seed, image])
    # mke2fs -d preserves builder credentials. Set target UID/GID explicitly.
    fields = []
    for path in sorted(seed.rglob("*")):
        relative = path.relative_to(seed).as_posix()
        uid = owners.get(relative, 0)
        fields.extend(f"set_inode_field /{relative} {field} {uid}\n" for field in ("uid", "gid"))
    subprocess.run(["debugfs", "-w", "-f", "-", image], input="".join(fields), text=True,
                   stdout=subprocess.DEVNULL, check=True, timeout=20)
    boot_media.run(work, ["e2fsck", "-f", "-n", image])
    return image


def new_disk(work, disk, args, instance, home_id, layout):
    seed = work / "seed"
    seed.mkdir()
    seed.chmod(0o755)
    (seed / "initialize-once").touch()
    (seed / "deployment-uuid").write_text(str(instance) + "\n")
    for name in ("initialize-once", "deployment-uuid"):
        (seed / name).chmod(0o444)
    host_config(seed, args)
    state = seeded_ext4(work, "state.ext4", "PSIOSSTATE", instance, layout.state_mib, seed, {})
    homes = work / "home-seed"
    homes.mkdir()
    homes.chmod(0o755)
    for name in ("root", "psios"):
        (homes / name).mkdir()
        (homes / name).chmod(0o700)
    home = seeded_ext4(work, "home.ext4", "PSIOSHOME", home_id, layout.home_mib, homes, {"psios": 1000})
    with disk.open("xb") as f:
        f.truncate(layout.disk_bytes)
    gpt = "label: gpt\nunit: sectors\n" + "".join(
        f"start={start * 2048},size={size * 2048},type={kind}\n" for start, size, kind in layout.partitions)
    subprocess.run(["sfdisk", "--quiet", disk], input=gpt, text=True, check=True, timeout=30)
    validate_layout(work, disk, layout)
    for index, partition in ((1, state), (2, home)):
        write_partition(disk, layout.partitions[index], partition, replace=False)


def copy_mib(src, dst, count, *, replace):
    copy_sparse(src, dst, count * MIB, replace=replace)


def copy_sparse(src, dst, size, *, replace):
    # New regions are already zero. During journal replay and EFI replacement,
    # omit a zero block only if the destination still contains those same bytes.
    zero = bytes(MIB)
    for offset in range(0, size, MIB):
        length = min(MIB, size - offset)
        chunk = src.read(length)
        if len(chunk) != length:
            raise ValueError("incomplete partition data")
        if chunk == zero[:length]:
            if not replace:
                dst.seek(length, os.SEEK_CUR)
                continue
            previous = dst.read(length)
            if len(previous) != length:
                raise ValueError("incomplete destination partition data")
            if previous == chunk:
                continue
            dst.seek(-length, os.SEEK_CUR)
        dst.write(chunk)


def write_partition(disk, partition, source, *, replace):
    start, size, _ = partition
    if source.stat().st_size != size * MIB:
        raise ValueError("partition image has unexpected size")
    with disk.open("r+b") as dst, source.open("rb") as src:
        dst.seek(start * MIB)
        copy_mib(src, dst, size, replace=replace)


def read_receipt(path):
    with boot_media.regular_file(path) as receipt:
        raw = receipt.read(16 * 1024 + 1)
    return parse_receipt(raw)


def parse_receipt(raw):
    if len(raw) > 16 * 1024:
        raise ValueError("previous build receipt exceeds size limit")
    prior = json.loads(raw)
    extended_formats = (E1000E_RECEIPT_FORMAT, SSH_RECEIPT_FORMAT, SLAAC_RECEIPT_FORMAT, RDNSS_RECEIPT_FORMAT)
    if isinstance(prior, dict) and (prior.get("format") == TRIAL_RECEIPT_FORMAT or "boot_mode" in prior):
        raise ValueError("isolated trial disks cannot be upgrade sources; use the original stopped deployment")
    if (not isinstance(prior, dict) or type(prior.get("format")) is not int
            or prior["format"] not in (*range(1, 7), *extended_formats)):
        raise ValueError("unsupported previous build receipt format")
    for name, pattern in (("source_commit", r"[0-9a-f]{40}"), ("efi_application_sha256", r"[0-9a-f]{64}")):
        if not isinstance(prior.get(name), str) or not re.fullmatch(pattern, prior[name]):
            raise ValueError(f"invalid previous build receipt field {name}")
    for name in ("state_uuid", "home_uuid"):
        value = prior.get(name)
        if not isinstance(value, str) or str(uuid.UUID(value)) != value:
            raise ValueError(f"invalid previous build receipt field {name}")
    if prior["state_uuid"] == prior["home_uuid"]:
        raise ValueError("previous deployment filesystems have identical UUIDs")
    if prior["format"] == 1 and "raw_disk_bytes" in prior:
        raise ValueError("larger raw capacity requires a geometry-bearing build receipt")
    layout = Layout.from_receipt(prior) if prior["format"] >= 2 else Layout()
    profiles = {4: (STATIC_IPV4,), 5: (STATIC_IPV6, STATIC_DUAL), SLAAC_RECEIPT_FORMAT: (SLAAC,),
                RDNSS_RECEIPT_FORMAT: (RDNSS,)}
    if prior["format"] in profiles:
        if prior.get("network_profile") not in profiles[prior["format"]]:
            raise ValueError("invalid network profile in previous build receipt")
    elif "network_profile" in prior:
        ssh_profiles = E1000E_STATIC_MODES if "wired_driver" in prior else VIRTIO_STATIC_MODES
        supported = {6: VIRTIO_STATIC_MODES, E1000E_RECEIPT_FORMAT: E1000E_STATIC_MODES,
                     SSH_RECEIPT_FORMAT: ssh_profiles}
        profile = prior["network_profile"]
        if not isinstance(profile, str) or profile not in supported.get(prior["format"], {}):
            raise ValueError("invalid network profile in previous build receipt")
    if prior["format"] == E1000E_RECEIPT_FORMAT or (
            prior["format"] == SSH_RECEIPT_FORMAT and "wired_driver" in prior):
        if prior.get("wired_driver") != "e1000e":
            raise ValueError("invalid wired driver in previous native build receipt")
    elif "wired_driver" in prior:
        raise ValueError("wired driver evidence requires build receipt format 8 or 9")
    if prior["format"] == SSH_RECEIPT_FORMAT or (
            prior["format"] in (SLAAC_RECEIPT_FORMAT, RDNSS_RECEIPT_FORMAT) and "ssh_console" in prior):
        if prior.get("ssh_console") != SSH_PROFILE:
            raise ValueError("invalid SSH console in previous build receipt")
    elif "ssh_console" in prior:
        raise ValueError("SSH console evidence requires build receipt format 9, 10 or 11")
    limits = None
    if prior["format"] == 6 or (prior["format"] in extended_formats and "core_limits" in prior):
        limits = CoreLimits.from_receipt(prior.get("core_limits"))
    elif "core_limits" in prior:
        raise ValueError("Core resource limits require build receipt format 6, 8, 9, 10 or 11")
    if prior["format"] == 3 or (prior["format"] >= 4 and "secure_boot" in prior):
        trust = prior.get("secure_boot")
        if (not isinstance(trust, dict) or set(trust) != {"scheme", "certificate_sha256", "efi_stub_sha256"}
                or trust["scheme"] != boot_media.SECURE_BOOT_SCHEME or any(
                    not isinstance(trust[key], str) or not re.fullmatch(r"[0-9a-f]{64}", trust[key])
                    for key in ("certificate_sha256", "efi_stub_sha256"))):
            raise ValueError("invalid Secure Boot evidence in previous build receipt")
    elif "secure_boot" in prior:
        raise ValueError("Secure Boot evidence requires build receipt format 3 or later")
    return prior, layout, limits


def read_gpt_copy(disk, header_lba, alternate_lba, entries_lba, usable):
    disk.seek(header_lba * SECTOR_BYTES)
    sector = disk.read(SECTOR_BYTES)
    if len(sector) != SECTOR_BYTES:
        raise ValueError("relocated image has an incomplete GPT header")
    (signature, revision, size, checksum, reserved, current, alternate, first, last, disk_id,
     entries, count, entry_size, entries_checksum) = GPT_HEADER.unpack_from(sector)
    if (signature != b"EFI PART" or revision != 0x10000 or not GPT_HEADER.size <= size <= SECTOR_BYTES
            or reserved != 0 or (current, alternate, entries) != (header_lba, alternate_lba, entries_lba)
            or (first, last) != usable or not 0 < count * entry_size <= MIB):
        raise ValueError("relocated image has a missing or inconsistent GPT header")
    if binascii.crc32(sector[:16] + bytes(4) + sector[20:size]) != checksum:
        raise ValueError("relocated image has an invalid GPT header checksum")
    table_bytes = count * entry_size
    offset = entries_lba * SECTOR_BYTES
    boundary = (first if header_lba == 1 else header_lba) * SECTOR_BYTES
    if entry_size < 128 or offset + table_bytes > boundary:
        raise ValueError("relocated GPT table exceeds its reserved area")
    disk.seek(offset)
    partitions = disk.read(table_bytes)
    if len(partitions) != table_bytes or binascii.crc32(partitions) != entries_checksum:
        raise ValueError("relocated image has an invalid GPT table checksum")
    return disk_id, count, entry_size, partitions


def verify_relocated_gpt(image, target_size, table):
    """Require two complete GPT copies with matching identities and partition tables."""
    last_sector = target_size // SECTOR_BYTES - 1
    usable = table["firstlba"], table["lastlba"]
    with open(image, "rb") as disk:
        protective = disk.read(SECTOR_BYTES)
        if (len(protective) != SECTOR_BYTES or protective[510:] != b"\x55\xaa" or protective[450] != 0xEE
                or struct.unpack_from("<II", protective, 454) != (1, last_sector)):
            raise ValueError("relocated image has an invalid protective MBR")
        primary = read_gpt_copy(disk, 1, last_sector, 2, usable)
        backup = read_gpt_copy(disk, last_sector, 1, usable[1] + 1, usable)
    if primary != backup or uuid.UUID(bytes_le=primary[0]) != uuid.UUID(table["id"]):
        raise ValueError("relocated image GPT copies do not agree")


def validate_layout(work, disk, layout, *, pass_fds=()):
    boot_media.run(work, ["sfdisk", "--verify", disk], pass_fds=pass_fds)
    limit = 16 * 1024
    with tempfile.TemporaryFile(dir=work) as listing:
        subprocess.run(["sfdisk", "--json", disk], stdin=subprocess.DEVNULL, stdout=listing,
                       check=True, timeout=30, pass_fds=pass_fds, preexec_fn=file_limit(limit))
        listing.seek(0)
        gpt = json.loads(listing.read(limit + 1))["partitiontable"]
    expected = [(start * 2048, size * 2048, kind) for start, size, kind in layout.partitions]
    if (gpt["label"], gpt["unit"], gpt["sectorsize"]) != ("gpt", "sectors", 512) or [
            (p["start"], p["size"], p["type"]) for p in gpt["partitions"]] != expected:
        raise ValueError("unsupported previous PsiOS GPT layout")
    if layout.raw_disk_bytes is not None:
        verify_relocated_gpt(disk, layout.disk_bytes, gpt)
    return gpt


def file_limit(max_bytes):
    soft, _ = resource.getrlimit(resource.RLIMIT_FSIZE)
    limit = max_bytes if soft == resource.RLIM_INFINITY else min(max_bytes, soft)
    return partial(resource.setrlimit, resource.RLIMIT_FSIZE, (limit, limit))


def filesystem_state(path, expected_uuid):
    with path.open("rb") as fs:
        fs.seek(1024)
        return filesystem_superblock(fs.read(1024), expected_uuid)


def filesystem_superblock(superblock, expected_uuid):
    if len(superblock) != 1024:
        raise ValueError("incomplete ext4 superblock")
    magic, state = struct.unpack_from("<HH", superblock, 56)
    if magic != 0xEF53 or str(uuid.UUID(bytes=superblock[104:120])) != expected_uuid:
        raise ValueError("ext4 partition does not match the previous deployment UUID")
    compat, incompat, ro_compat = struct.unpack_from("<III", superblock, 92)
    journal_inode, journal_device = struct.unpack_from("<II", superblock, 224)
    if (compat & 4) == 0 or journal_inode == 0 or journal_device != 0 or any(superblock[208:224]):
        raise ValueError("expected an ext4 filesystem with an internal journal")
    # These are EXT4_VALID_FS, HAS_JOURNAL, RECOVER and ORPHAN_PRESENT.
    return state, bool(incompat & 4 or ro_compat & 0x10000)


def check_filesystem(work, disk, index, expected_uuid, layout):
    partition = layout.partitions[index]
    start, size, _ = partition
    fs = work / f"previous-partition-{index + 1}.ext4"
    with disk.open("rb") as source, fs.open("xb") as out:
        source.seek(start * MIB)
        copy_mib(source, out, size, replace=False)
        out.truncate(size * MIB)
    state, pending = filesystem_state(fs, expected_uuid)
    if pending and (state & ~1) == 0:
        replay = subprocess.run(["e2fsck", "-p", "-E", "journal_only", fs], stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, timeout=90, preexec_fn=file_limit(size * MIB))
        if replay.returncode not in (0, 1):
            raise ValueError(f"could not replay partition {index + 1} ext4 journal")
        state, still_pending = filesystem_state(fs, expected_uuid)
    else:
        still_pending = pending
    if state != 1 or still_pending:
        raise ValueError(f"partition {index + 1} requires filesystem repair before upgrade")
    boot_media.run(work, ["e2fsck", "-f", "-n", fs])
    if pending:
        write_partition(disk, partition, fs, replace=True)
    return fs, pending


def read_partition_file(work, fs, member, limit):
    name = Path(member).name
    subprocess.run(["debugfs", "-R", f"dump {member} {name}", fs], cwd=work,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True,
                   timeout=20, preexec_fn=file_limit(limit + 1))
    with boot_media.regular_file(work / name) as f:
        data = f.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"previous disk file exceeds size limit: {member}")
    return data


def read_root_file(work, fs, member, limit, modes):
    inode = subprocess.check_output(["debugfs", "-R", f"stat {member}", fs], cwd=work,
                                    stderr=subprocess.DEVNULL, text=True, timeout=20)
    if (not re.search(rf"\bType:\s+regular\s+Mode:\s+{modes}\b", inode)
            or not re.search(r"\bUser:\s+0\s+Group:\s+0\b", inode)
            or not re.search(r"^Links:\s+1\b", inode, re.MULTILINE)):
        raise ValueError(f"unsafe ownership, permissions or link count for {member}")
    return read_partition_file(work, fs, member, limit)


def source_snapshot(work, disk, source_fd, prior, layout, signing):
    disk_size = layout.disk_bytes
    before = os.fstat(source_fd)
    flags = fcntl.fcntl(source_fd, fcntl.F_GETFL)
    if not stat.S_ISREG(before.st_mode) or flags & os.O_PATH or flags & os.O_ACCMODE != os.O_RDONLY:
        raise ValueError("previous PsiOS disk requires a regular read-only descriptor")
    if before.st_size != disk_size:
        raise ValueError("unsupported previous PsiOS disk size")
    options = f"driver=raw,file.driver=file,file.filename=/proc/self/fd/{source_fd},file.locking=on"
    subprocess.run(["qemu-img", "convert", "--image-opts", "-m", "1", "-O", "raw", options, disk],
                   pass_fds=(source_fd,), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, check=True,
                   timeout=90, preexec_fn=file_limit(disk_size))
    after = os.fstat(source_fd)
    if (after.st_size, after.st_mtime_ns, after.st_ctime_ns) != (before.st_size, before.st_mtime_ns, before.st_ctime_ns):
        raise ValueError("previous disk changed during capture")
    if disk.stat().st_size != disk_size:
        raise ValueError("incomplete previous disk capture")
    digest = sha256(disk)
    validate_layout(work, disk, layout)
    old_efi = work / "previous.efi"
    boot_media.run(work, ["mcopy", "-i", f"{disk.name}@@{ESP[0] * MIB}", EFI_PATH, old_efi])
    if sha256(old_efi) != prior["efi_application_sha256"]:
        raise ValueError("previous EFI application does not match the trusted build receipt")
    if prior.get("secure_boot"):
        signing.verify(work, old_efi)
    state, replayed_state = check_filesystem(work, disk, 1, prior["state_uuid"], layout)
    homes, replayed_home = check_filesystem(work, disk, 2, prior["home_uuid"], layout)
    homes.unlink()
    marker = read_partition_file(work, state, "/deployment-uuid", 64)
    if marker != (prior["state_uuid"] + "\n").encode():
        raise ValueError("state partition deployment marker does not match the prior receipt")
    core_config = read_root_file(work, state, "/etc/psios/core.json", 64 * 1024, "0(?:444|644)")
    cfg = json.loads(core_config)
    validate_persisted_network(work, state, prior)
    if "ssh_console" in prior:
        validate_persisted_ssh(work, state)
    if "core_limits" in prior:
        inittab = read_root_file(work, state, "/etc/inittab", 16 * 1024, "0(?:600|644)").split(b"\n")
        if inittab.count(LIMITED_SERVICE.encode()) != 1:
            raise ValueError("invalid persisted Core service launcher for resource limits")
    state.unlink()
    if not isinstance(cfg, dict) or (cfg.get("state_dir"), cfg.get("witness_dir")) != ("/var/lib/psios", "/var/lib/psios.witness"):
        raise ValueError("offline upgrade requires the native state and witness locations")
    replayed = [part for part, replay in ((2, replayed_state), (3, replayed_home)) if replay]
    return digest, replayed, core_config


def copy_budget_mib(value):
    try:
        mib = int(value)
    except ValueError:
        mib = 0
    if not 1 <= mib <= 256:
        raise argparse.ArgumentTypeError("authority copy limit must be 1–256 MiB")
    return mib


def verify_authority(work, disk, core_config, max_copy_mib):
    core = work / "bin" / "psiosd"
    config = work / "persisted-core.json"
    with config.open("xb") as file:
        config.chmod(0o600)
        file.write(core_config)
    report = json.loads(subprocess.check_output([
        sys.executable, HERE.parent / "rescue" / "inspect.py", disk,
        "--iso", work / "base.iso", "--core", core, "--core-sha256", sha256(core),
        "--partition", "2", "--state", "/psios", "--uid", "1000",
        "--migration-config", config, "--max-copy-mib", str(max_copy_mib),
    ], timeout=210))
    copied = report["copied_bytes"]
    if type(copied) is not int or not 0 < copied <= max_copy_mib * MIB:
        raise ValueError("source authority report exceeds its verified copy budget")
    report["max_copy_mib"] = max_copy_mib
    report["migration_rehearsal"]["configuration_sha256"] = hashlib.sha256(core_config).hexdigest()
    return report


def prepare_trial_disk(work, disk, layout, configuration, *, secure_boot_required=False):
    trial = TrialInput(uuid.uuid4(), hashlib.sha256(configuration).hexdigest(), secure_boot_required)
    # A trial EFI payload can only open its own copied authority writable.
    boot_media.run(work, ["sfdisk", "--disk-id", disk, str(trial.disk_uuid)])
    validate_layout(work, disk, layout)
    return trial


def production_receipt_format(properties):
    if properties.get("network_profile") == RDNSS:
        return RDNSS_RECEIPT_FORMAT
    if properties.get("network_profile") == SLAAC:
        return SLAAC_RECEIPT_FORMAT
    if "ssh_console" in properties:
        return SSH_RECEIPT_FORMAT
    if "wired_driver" in properties:
        return E1000E_RECEIPT_FORMAT
    if "core_limits" in properties:
        return 6
    static_formats = {STATIC_IPV4: 4, STATIC_IPV6: 5, STATIC_DUAL: 5}
    return static_formats.get(properties.get("network_profile"), 3 if "secure_boot" in properties else 2)


def finish_disk(work, disk, output, *, signing, upgrade, tools):
    manifest_path = work / "psios-deployment.json"
    manifest = json.loads(manifest_path.read_bytes())
    if signing:
        efi = boot_media.secure_efi_application(work, CMDLINE, signing, tools=tools)
    else:
        efi = boot_media.efi_application(work, CMDLINE, tools=tools)
    esp = work / "esp.fat"
    with esp.open("xb") as f:
        f.truncate(ESP[1] * MIB)
    boot_media.run(work, ["mkfs.fat", "-F", "32", "--invariant", "-n", "PSIOSBOOT", esp], tools=tools)
    boot_media.run(work, ["mmd", "-i", esp, "::/EFI", "::/EFI/BOOT"], tools=tools)
    boot_media.run(work, ["mcopy", "-i", esp, efi, EFI_PATH], tools=tools)
    write_partition(disk, ESP, esp, replace=upgrade is not None)
    with disk.open("rb") as completed:
        os.fsync(completed.fileno())
    receipt = {
        "format": production_receipt_format(manifest),
        "image": str(output), "source_commit": manifest["core_source_commit"],
        "geometry_mib": manifest["geometry_mib"], "state_uuid": manifest["required_filesystems"]["state"],
        "home_uuid": manifest["required_filesystems"]["home"],
        "deployment_manifest_sha256": sha256(manifest_path),
        "efi_application_sha256": sha256(efi), "installed_initramfs_sha256": sha256(work / "initrd"),
        "initial_image_sha256": sha256(disk),
    }
    for name in ("secure_boot", "network_profile", "wired_driver", "core_limits", "ssh_console", "raw_disk_bytes"):
        if name in manifest:
            receipt[name] = manifest[name]
    if "boot_mode" in manifest:
        receipt.update(format=TRIAL_RECEIPT_FORMAT, boot_mode=manifest["boot_mode"],
                       trial_disk_uuid=manifest["trial_disk_uuid"])
    if "secure_boot_enforcement_required" in manifest:
        receipt["secure_boot_enforcement_required"] = True
    if upgrade is not None:
        receipt["upgrade"] = deepcopy(upgrade)
    return receipt


def build_update_pair(work: Path, args, source_fd: int) -> PreparedUpdate:
    """Prepare two private EFI variants of one stopped source; publish neither."""
    prior, layout, core_limits = read_receipt(args.previous_receipt)
    layout = layout.for_source(getattr(args, "source_size_bytes", None))
    work = work.absolute()
    requested = tuple(getattr(args, name, None) for name in (
        "secure_boot_key", "secure_boot_cert", "efi_stub", "efi_stub_sha256"))
    existing = prior.get("secure_boot")
    if not existing and any(value is not None for value in requested):
        raise ValueError("a signed staged update requires a signed original deployment")
    signing = None
    tools = boot_media.BootTools()
    if existing:
        if not all(requested):
            raise ValueError("a signed original deployment requires the Secure Boot signing inputs")
        signing = boot_media.prepare_secure_boot(work, *requested, tools=tools)
        if signing.certificate_sha256 != existing["certificate_sha256"]:
            raise ValueError("a signed staged update requires the original enrolled certificate")
    candidate_work, trial_work = work / "candidate", work / "trial"
    for directory in (candidate_work, trial_work):
        directory.mkdir(mode=0o700)
    candidate_disk, trial_disk = candidate_work / "installed.raw", trial_work / "installed.raw"
    source_digest, replayed, configuration = source_snapshot(
        candidate_work, candidate_disk, source_fd, prior, layout, signing)
    normalized = sha256(candidate_disk)
    with candidate_disk.open("rb") as frozen, trial_disk.open("xb") as clone:
        copy_sparse(frozen, clone, layout.disk_bytes, replace=False)
        clone.truncate(layout.disk_bytes)
    if sha256(trial_disk) != normalized:
        raise ValueError("trial disk clone does not match the normalized source")
    for disk in (candidate_disk, trial_disk):
        disk.chmod(0o600)
    trial = prepare_trial_disk(trial_work, trial_disk, layout, configuration,
                               secure_boot_required=signing is not None)

    wired_driver = prior.get("wired_driver")
    ssh_enabled = "ssh_console" in prior
    network_profile = prior.get("network_profile")
    base = prepare_boot_inputs(candidate_work, args, wired_driver=wired_driver, ssh_enabled=ssh_enabled,
                               network_profile=network_profile, tools=tools)
    (trial_work / "bin").mkdir()
    # Share the pinned input kernel and binaries, but each initrd is extended independently.
    boot_inputs = ("kernel", "initrd", *core_programs(candidate_work), "psios-source", ZONE_ARCHIVE, TIMEZONE_COMMAND)
    if wired_driver:
        boot_inputs += (E1000E_MODULE,)
    if ssh_enabled:
        boot_inputs += SSH_PROGRAMS
    if network_profile == RDNSS:
        boot_inputs += (RDNSS_PROGRAM,)
    for name in boot_inputs:
        (trial_work / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(candidate_work / name, trial_work / name)
    instance, home_id = prior["state_uuid"], prior["home_uuid"]
    for directory, mode in ((candidate_work, None), (trial_work, trial)):
        assemble_ramfs(directory, args, base, instance, home_id, layout, signing, network_profile,
                       core_limits=core_limits, trial=mode, wired_driver=wired_driver, ssh_enabled=ssh_enabled,
                       require_secure_boot=signing is not None and mode is None, tools=tools)
    candidate_manifest = json.loads((candidate_work / "psios-deployment.json").read_bytes())
    trial_manifest = json.loads((trial_work / "psios-deployment.json").read_bytes())
    del trial_manifest["boot_mode"], trial_manifest["trial_disk_uuid"]
    del trial_manifest["payload_sha256"]["/psios-trial-config.sha256"]
    del trial_manifest["payload_sha256"]["/psios-trial-disk-id"]
    if signing:
        del trial_manifest["secure_boot_enforcement_required"]
        del trial_manifest["payload_sha256"]["/psios-trial-secure-boot-required"]
        del candidate_manifest["secure_boot_enforcement_required"]
        del candidate_manifest["payload_sha256"]["/psios-secure-boot-required"]
    if trial_manifest != candidate_manifest:
        raise ValueError("trial and candidate boot inputs disagree")

    authority = verify_authority(candidate_work, candidate_disk, configuration, args.max_copy_mib)
    upgrade = {"previous_image_sha256": source_digest, "previous_source_commit": prior["source_commit"],
               "normalized_source_sha256": normalized, "verified_authority": authority,
               "replayed_ext4_partitions": replayed}
    candidate_receipt = finish_disk(candidate_work, candidate_disk, candidate_disk,
                                    signing=signing, upgrade=upgrade, tools=tools)
    trial_receipt = finish_disk(trial_work, trial_disk, trial_disk, signing=signing, upgrade=upgrade, tools=tools)
    candidate_receipt["boot_assembly_tools"] = trial_receipt["boot_assembly_tools"] = tools.receipt()
    return PreparedUpdate(candidate_disk, trial_disk, candidate_receipt, trial_receipt,
                          candidate_work / "bin/psiosd", configuration, normalized,
                          candidate_work / "base.iso", args.max_copy_mib, tools)


def build(args):
    output = args.output.absolute()
    if os.path.lexists(output):
        raise ValueError("output already exists")
    with tempfile.TemporaryDirectory(dir=output.parent, prefix=".psios-disk-") as tmp:
        work = Path(tmp)
        disk = work / "installed.raw"
        prior = None
        trial = None
        network_profile = args.network_profile
        wired_driver = args.wired_driver
        ssh_enabled = args.ssh_authorized_key is not None
        core_limits = args.core_limits
        signing = None
        tools = boot_media.BootTools()
        if args.secure_boot_key:
            signing = boot_media.prepare_secure_boot(work, args.secure_boot_key, args.secure_boot_cert,
                                                    args.efi_stub, args.efi_stub_sha256, tools=tools)
        if args.upgrade_from:
            prior, layout, core_limits = read_receipt(args.previous_receipt)
            layout = layout.for_source(args.source_size_bytes)
            network_profile = prior.get("network_profile")
            wired_driver = prior.get("wired_driver")
            ssh_enabled = "ssh_console" in prior
            if prior.get("secure_boot") and (signing is None or
                    signing.certificate_sha256 != prior["secure_boot"]["certificate_sha256"]):
                raise ValueError("a Secure Boot upgrade requires the existing signing certificate")
            instance, home_id = prior["state_uuid"], prior["home_uuid"]
            with boot_media.regular_file(args.upgrade_from) as source:
                source_digest, replayed, core_config = source_snapshot(work, disk, source.fileno(), prior, layout, signing)
            if args.trial_boot:
                trial = prepare_trial_disk(work, disk, layout, core_config)
        else:
            layout = Layout(MIN_STATE_MIB if args.state_mib is None else args.state_mib,
                            MIN_HOME_MIB if args.home_mib is None else args.home_mib)
            instance, home_id = str(uuid.uuid4()), str(uuid.uuid4())
        ramfs(work, args, instance, home_id, layout, signing, network_profile, core_limits=core_limits,
              trial=trial, wired_driver=wired_driver, ssh_enabled=ssh_enabled, tools=tools)
        if not prior:
            new_disk(work, disk, args, instance, home_id, layout)
        disk.chmod(0o600)
        upgrade = None
        if prior:
            upgrade = {
                "previous_image_sha256": source_digest,
                "previous_source_commit": prior["source_commit"],
                "verified_authority": verify_authority(work, disk, core_config, args.max_copy_mib),
                "replayed_ext4_partitions": replayed,
            }
        receipt = finish_disk(work, disk, output, signing=signing, upgrade=upgrade, tools=tools)
        receipt["boot_assembly_tools"] = tools.receipt()
        # No existing image or block device is ever an install target.
        os.link(disk, output)
        fd = os.open(output.parent, os.O_DIRECTORY | os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    print(json.dumps(receipt))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--kernel-iso", required=True, type=Path)
    parser.add_argument("--core-archive", required=True, type=Path)
    parser.add_argument("--core-sha256", required=True, help="complete archive digest obtained after provenance verification")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--console-hash", type=Path, help="private file containing an operator SHA-512 crypt hash")
    mode.add_argument("--upgrade-from", type=Path, help="stopped native development disk to copy without changes")
    parser.add_argument("--previous-receipt", type=Path, help="trusted build receipt for the stopped disk")
    parser.add_argument("--source-size-bytes", type=int, help="installed raw capacity from an approved larger-target plan")
    parser.add_argument("--hostname", help="single DNS-label hostname for a new disk")
    parser.add_argument("--timezone", help="IANA time zone for a new disk (default UTC)")
    parser.add_argument("--trial-boot", action="store_true", help="create an isolated non-activating EFI upgrade candidate")
    parser.add_argument("--max-copy-mib", type=copy_budget_mib,
                        help="authority database and journals to copy during upgrade (default 64 MiB)")
    parser.add_argument("--root-console-hash", type=Path, help="optional private hash for local system administration; default locks root")
    parser.add_argument("--ssh-authorized-key", type=Path, help="authorize an Ed25519 public key for the psios SSH console")
    boot_media.add_signing_arguments(parser)
    network = parser.add_mutually_exclusive_group()
    network.add_argument("--dhcp-mac", type=unicast_mac, help="enable IPv4 DHCP on the matching NIC of a new disk")
    network.add_argument("--static-mac", type=unicast_mac, help="enable static networking on the matching NIC of a new disk")
    network.add_argument("--slaac-mac", type=unicast_mac, help="enable IPv6 router discovery on the matching virtio NIC of a new disk")
    parser.add_argument("--dhcp-driver", choices=("virtio", "e1000e"),
                        help="kernel driver for the DHCP NIC (default virtio)")
    parser.add_argument("--static-driver", choices=("virtio", "e1000e"),
                        help="kernel driver for the static NIC (default virtio)")
    parser.add_argument("--static-address", type=ipaddress.IPv4Interface, help="static IPv4 address and prefix")
    parser.add_argument("--static-gateway", type=ipaddress.IPv4Address, help="optional IPv4 gateway on the static subnet")
    parser.add_argument("--static-dns", type=ipaddress.IPv4Address, action="append", default=[], help="optional IPv4 DNS server, repeat up to three times")
    parser.add_argument("--static-ipv6-address", type=ipaddress.IPv6Interface, help="static IPv6 address and prefix")
    parser.add_argument("--static-ipv6-gateway", type=ipaddress.IPv6Address, help="optional IPv6 on-link or link-local gateway")
    parser.add_argument("--static-ipv6-dns", type=ipaddress.IPv6Address, action="append", default=[], help="optional unscoped IPv6 DNS server")
    parser.add_argument("--slaac-dns", type=ipaddress.IPv6Address, action="append", default=[], help="explicit IPv6 DNS server for SLAAC")
    parser.add_argument("--slaac-rdnss", action="store_true", help="accept DNS servers advertised by the IPv6 router")
    parser.add_argument("--state-mib", type=int, help=f"state partition size of a new disk (default {MIN_STATE_MIB} MiB)")
    parser.add_argument("--home-mib", type=int, help=f"home partition size of a new disk (default {MIN_HOME_MIB} MiB)")
    core_options = parser.add_argument_group("new disk Core cgroup limits (provide all three to enable)")
    core_options.add_argument("--core-cpu-millicores", type=int, help="CPU quota of the Core daemon in millicores")
    core_options.add_argument("--core-memory-mib", type=int, help="hard memory limit in MiB with swap disabled for Core")
    core_options.add_argument("--core-pids-max", type=int, help="hard task count limit for Core and its descendants")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{64}", args.core_sha256):
        parser.error("invalid Core archive SHA-256")
    if args.source_size_bytes is not None and not args.upgrade_from:
        parser.error("--source-size-bytes requires --upgrade-from")
    if args.hostname is not None:
        if args.upgrade_from:
            parser.error("upgrade preserves /etc/hostname from the source")
        if not re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?", args.hostname):
            parser.error("hostname must be a single DNS label of 1 to 63 ASCII letters, digits or hyphens")
    if args.timezone is not None:
        if args.upgrade_from:
            parser.error("upgrade preserves /etc/localtime from the source")
        if not timezone_name(args.timezone):
            parser.error("invalid time zone name")
    if args.dhcp_driver and not args.dhcp_mac:
        parser.error("--dhcp-driver requires --dhcp-mac on a new disk")
    if args.static_driver and not args.static_mac:
        parser.error("--static-driver requires --static-mac on a new disk")
    args.wired_driver = "e1000e" if "e1000e" in (args.dhcp_driver, args.static_driver) else None
    boot_media.signing_requested(args, parser)
    args.static_network, args.static_ipv6_network = static_config(args, parser)
    args.network_profile = None
    if args.static_network:
        args.network_profile = STATIC_DUAL if args.static_ipv6_network else STATIC_IPV4
    elif args.static_ipv6_network:
        args.network_profile = STATIC_IPV6
    if args.network_profile and args.wired_driver:
        args.network_profile = args.network_profile.replace("virtio-", "e1000e-", 1)
    if args.slaac_dns and args.slaac_mac is None:
        parser.error("--slaac-dns requires --slaac-mac on a new disk")
    if args.slaac_rdnss and (args.slaac_mac is None or args.slaac_dns):
        parser.error("--slaac-rdnss requires --slaac-mac and excludes --slaac-dns")
    if args.slaac_mac is not None:
        args.network_profile = RDNSS if args.slaac_rdnss else SLAAC
        try:
            args.slaac_network = slaac_profile(args.slaac_mac, args.slaac_dns)
        except ValueError as error:
            parser.error(str(error))
    if args.ssh_authorized_key:
        if args.upgrade_from or not (args.dhcp_mac or args.network_profile):
            parser.error("--ssh-authorized-key requires an explicit network profile on a new disk")
        with boot_media.regular_file(args.ssh_authorized_key) as key:
            args.ssh_authorized_key = ssh_public_key(key.read(513))
    if args.upgrade_from and (not args.previous_receipt or args.root_console_hash or args.dhcp_mac or args.network_profile):
        parser.error("upgrade requires --previous-receipt and preserves console credentials and network configuration")
    if args.upgrade_from and (args.state_mib is not None or args.home_mib is not None):
        parser.error("upgrade preserves the partition sizes from its previous build receipt")
    if args.console_hash and args.previous_receipt:
        parser.error("--previous-receipt requires --upgrade-from")
    if args.trial_boot and not args.upgrade_from:
        parser.error("--trial-boot requires --upgrade-from")
    if args.max_copy_mib is not None and not args.upgrade_from:
        parser.error("--max-copy-mib requires --upgrade-from")
    if args.max_copy_mib is None:
        args.max_copy_mib = 64
    limits = (args.core_cpu_millicores, args.core_memory_mib, args.core_pids_max)
    args.core_limits = None
    if any(value is not None for value in limits):
        if args.upgrade_from:
            parser.error("upgrade preserves Core resource limits from its previous build receipt")
        if any(value is None for value in limits):
            parser.error("provide all three Core resource limits together")
        args.core_limits = CoreLimits(*limits)
    build(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, tarfile.TarError) as error:
        sys.exit(str(error))
