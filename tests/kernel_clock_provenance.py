"""Exercise clock source custody in an explicitly private Linux mount namespace."""
import argparse
from contextlib import contextmanager
import os
from pathlib import Path
import signal
import subprocess
import uuid

from support import Failure, Node


BOOT = Path("/proc/sys/kernel/random/boot_id")


def mount(*arguments):
    subprocess.run(["mount", *arguments], check=True, timeout=8)


@contextmanager
def bound(source, destination, *, read_only=True):
    mount("--bind", source, destination)
    try:
        if read_only:
            mount("-o", "remount,bind,ro,nosuid,nodev,noexec", destination)
        yield
    finally:
        subprocess.run(["umount", destination], check=True, timeout=8)


@contextmanager
def in_memory_directory(directory):
    directory.mkdir(mode=0o700)
    mount("-t", "tmpfs", "-o", "mode=0700,size=1m,nodev,nosuid,noexec", "tmpfs", directory)
    try:
        yield
    finally:
        subprocess.run(["umount", directory], check=True, timeout=8)


def mount_fields(path):
    with path.open("rb") as source:
        name = Path(f"/proc/self/fdinfo/{source.fileno()}")
        mount_id = next(line.split()[1] for line in name.read_text().splitlines() if line.startswith("mnt_id:"))
    line = next(line for line in Path("/proc/self/mountinfo").read_text().splitlines()
                if line.startswith(mount_id + " "))
    prefix, suffix = map(str.split, line.split(" - ", 1))
    return prefix[3:6], suffix[0]


def require_private_mounts():
    namespace = Path("/proc/self/ns/mnt").readlink()
    parent = Path(f"/proc/{os.getppid()}/ns/mnt").readlink()
    assert namespace != parent, "clock fixture requires a private mount namespace"
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = line.split(" - ", 1)[0].split()
        if fields[4] in ("/", "/proc", "/proc/sys", str(BOOT)):
            assert not any(field.startswith("shared:") for field in fields[6:]), line


def issue(node, actor):
    return node.call("Grant.Issue", {"principal_id": actor["principal_id"], "units": 1, "ttl_seconds": 90,
        "actions": ["system.status"], "tool_targets": ["profile-local"]})["delegation_ref"]["resource_id"]


def status(node, actor, delegation):
    return node.call("Tool.Call", {"tool": "system.status", "input": {}, "delegation_id": delegation}, actor["token"])


def denied(expected, call):
    try:
        call()
    except Failure as failure:
        assert failure.code in expected, failure
    else:
        raise AssertionError("clock operation accepted an unverified source")


def verify_native_read_only(binary):
    # The bind is deliberately nonrecursive. In LXC this shows the underlying
    # native boot ID through the same kind of read-only procfs subtree as systemd.
    with bound("/proc/sys", "/proc/sys"):
        (root, mountpoint, options), kind = mount_fields(BOOT)
        assert (root, mountpoint, kind) == ("/sys", "/proc/sys", "proc"), (root, mountpoint, kind)
        assert options.split(",")[0] == "ro", options
        node = Node(binary)
        try:
            actor = node.call("Agent.Register", {"name": "clock-native-read-only"})
            assert node.call("Node.Get", {})["state"]["boot_id"] == BOOT.read_text().strip()
            assert status(node, actor, issue(node, actor))["authority_id"]
        finally:
            node.close()


def verify_bind_and_refusal(binary):
    node = Node(binary)
    try:
        expected = node.call("Node.Get", {})["state"]["boot_id"]
        actor = node.call("Agent.Register", {"name": "clock-source-mask"})
        original = issue(node, actor)
        baseline = status(node, actor, original)["authority_id"]
        with in_memory_directory(node.base / "boot-candidates"):
            tree = node.base / "boot-candidates"
            marker = tree / ".lxc-boot-id"
            marker.write_text(expected + "\n")
            marker.chmod(0o444)
            with bound(marker, BOOT):
                (root, mountpoint, options), kind = mount_fields(BOOT)
                assert (root, mountpoint, kind) == ("/.lxc-boot-id", str(BOOT), "tmpfs")
                assert set(options.split(",")) >= {"ro", "nosuid", "nodev", "noexec"}
                assert status(node, actor, original)["authority_id"] == baseline
                assert status(node, actor, issue(node, actor))["authority_id"] == baseline

            counterfeit = tree / "foreign-boot-id"
            counterfeit.write_text(str(uuid.uuid4()) + "\n")
            counterfeit.chmod(0o444)
            # Correct UUID syntax, ownership, size and flags still do not prove origin.
            with bound(counterfeit, BOOT):
                denied(("UnsupportedCapability",), lambda: status(node, actor, original))
            fifo = tree / "no-writer"
            os.mkfifo(fifo, mode=0o600)
            with bound(fifo, BOOT):
                assert BOOT.is_fifo()
                denied(("UnsupportedCapability",), lambda: issue(node, actor))
            foreign_threads = tree / "foreign-threads"
            foreign_threads.mkdir()
            with bound(foreign_threads, Path(f"/proc/{node.process.pid}/task")):
                denied(("UnsupportedCapability", "StorageUnavailable"), lambda: issue(node, actor))
        assert status(node, actor, original)["authority_id"] == baseline
        node.stop()
    except BaseException:
        node.stop(signal.SIGKILL)
        raise
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    require_private_mounts()
    verify_native_read_only(args.bin_dir)
    verify_bind_and_refusal(args.bin_dir)
    print("PASS native read-only procfs, exact LXC boot-ID profile, foreign boot/FIFO/time-source refusal")
