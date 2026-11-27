"""Exercise actual IPv6 link-local TCP routing between isolated Linux namespaces."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import time

from support import Node, target
from test_network_probe import TOOL


def ip(*args):
    subprocess.run(["ip", *args], check=True, timeout=8)


def line(process):
    if not select.select([process.stdout], [], [], 8)[0]:
        raise TimeoutError("peer namespace did not report readiness")
    return process.stdout.readline().decode().strip()


def peer():
    print("namespace ready", flush=True)
    assert sys.stdin.readline() == "configure\n"
    ip("link", "set", "dev", "probe-peer", "addrgenmode", "none")
    ip("link", "set", "probe-peer", "up")
    ip("-6", "address", "add", "fe80::705/64", "dev", "probe-peer", "nodad")
    scope = socket.if_nametoindex("probe-peer")
    with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as listener:
        listener.bind(("fe80::705", 0, 0, scope))
        listener.listen()
        listener.settimeout(12)
        print(listener.getsockname()[1], flush=True)
        connection, _ = listener.accept()
        with connection:
            connection.settimeout(4)
            assert connection.recv(1) == b"", "the target received unexpected application data"


def source_addresses(mode):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        device, = json.loads(subprocess.check_output(["ip", "-j", "-6", "addr", "show", "dev", "probe-local"]))
        addresses = {ipaddress.IPv6Address(item["local"]) for item in device["addr_info"]
                     if item["scope"] == "link" and not item.get("tentative") and not item.get("dadfailed")}
        if len(addresses) == (1 if mode == "none" else 2):
            assert ipaddress.IPv6Address("fe80::704") in addresses, addresses
            return addresses
        time.sleep(0.05)
    raise AssertionError(f"unexpected link-local address candidates for addrgenmode={mode}: {addresses}")


def exercise(mode):
    links = json.loads(subprocess.check_output(["ip", "-j", "link", "show"]))
    if os.geteuid() != 0 or [link["ifname"] for link in links] != ["lo"]:
        raise ValueError("run the test as root inside a fresh Linux network namespace")
    ip("link", "set", "lo", "up")
    with subprocess.Popen(["unshare", "--net", sys.executable, "-B", Path(__file__).resolve(), "--peer"],
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0) as process:
        try:
            assert line(process) == "namespace ready"
            ip("link", "add", "probe-local", "type", "veth", "peer", "name", "probe-peer")
            ip("link", "set", "probe-peer", "netns", str(process.pid))
            ip("link", "set", "dev", "probe-local", "addrgenmode", mode)
            ip("link", "set", "probe-local", "up")
            ip("-6", "address", "add", "fe80::704/64", "dev", "probe-local", "nodad")
            ifindex = socket.if_nametoindex("probe-local")
            process.stdin.write(b"configure\n")
            port = int(line(process))
            sources = {int(source) for source in source_addresses(mode)}
            address = f"[fe80::705%{ifindex}]:{port}"
            node = Node()
            try:
                node.stop()
                config = json.loads(node.config.read_text())
                config["network_probes"] = [dict(name="peer", endpoint=address)]
                node.config.write_text(json.dumps(config))
                node.start()
                peer_ref, = node.call("Resource.List", {"kind": "NetworkProbe"})["items"]
                delegation = node.call("Grant.Issue", {
                    "principal_id": "local-owner", "actions": [TOOL, "task.submit", "task.read"],
                    "tool_targets": [peer_ref["resource_id"]], "units": 1, "ttl_seconds": 60,
                })["delegation_ref"]["resource_id"]
                request = node.prepare(TOOL, target(peer_ref), delegation=delegation)
                task = node.wait(node.call("Task.Submit", request)["task_id"])
                assert task["lifecycle"] == "Succeeded", task
                value = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
                assert value["network_namespace_inode"] == str(os.stat("/proc/self/ns/net").st_ino)
                proof = value["observation"]
                assert proof["status"] == "Connected" and proof["endpoint"] == address, proof
                assert proof["tcp"]["status"] == "Connected", proof
                host, separator, port_text = proof["tcp"]["local_address"].rpartition("]:")
                assert host.startswith("[") and separator and 0 < int(port_text) <= 65535, proof
                source = ipaddress.IPv6Address(host[1:])
                assert source.scope_id == str(ifindex) and int(source) in sources, proof
            finally:
                node.close()
            assert process.wait(5) == 0
            print(f"PASS scoped IPv6 TCP selected a source on the authorized veth (addrgenmode={mode})")
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--addrgenmode", choices=("none", "eui64"), default="eui64")
    parser.add_argument("--peer", action="store_true", help=argparse.SUPPRESS)
    arguments = parser.parse_args()
    if arguments.peer:
        peer()
    else:
        exercise(arguments.addrgenmode)
