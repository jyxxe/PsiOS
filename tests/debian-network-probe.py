"""Verify both TCP families through the installed, unprivileged psiosd unit."""

import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import socket
import subprocess
import time

from support import target


def verify(config, service, user, client):
    command = ["runuser", "-u", user, "--", client, "--config", config]

    def ctl(*arguments, params=None):
        raw = None if params is None else json.dumps(params).encode()
        return json.loads(subprocess.check_output([*command, *arguments], input=raw, timeout=30))

    def call(method, params):
        return ctl("call", method, params=params)

    def identity():
        caller = ctl("whoami")
        return caller["principal_id"], caller["authority_id"]

    def restart():
        subprocess.run(["systemctl", "restart", service], check=True, timeout=30)
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            try:
                return identity()
            except subprocess.CalledProcessError:
                time.sleep(0.1)
        raise AssertionError("packaged Core did not become ready after restarting")

    original = config.read_bytes()
    original_identity = identity()
    with ExitStack() as stack:
        listeners = {}
        probes = []
        for name, family, ip in (("package-v4", socket.AF_INET, "127.0.0.1"),
                                 ("package-v6", socket.AF_INET6, "::1")):
            listener = stack.enter_context(socket.socket(family, socket.SOCK_STREAM))
            listener.bind((ip, 0))
            listener.listen(1)
            listener.settimeout(6)
            port = listener.getsockname()[1]
            endpoint = f"[{ip}]:{port}" if family == socket.AF_INET6 else f"{ip}:{port}"
            probes.append({"name": name, "endpoint": endpoint})
            listeners[name] = (listener, endpoint)

        try:
            settings = json.loads(original)
            settings["network_probes"] = [*settings.get("network_probes", []), *probes]
            config.write_text(json.dumps(settings) + "\n")
            assert restart() == original_identity
            references = {}
            for ref in call("Resource.List", {"kind": "NetworkProbe"})["items"]:
                name = call("Resource.Get", {"id": ref["resource_id"]})["value"]["name"]
                if name in listeners:
                    references[name] = ref
            assert references.keys() == listeners.keys(), references
            tool = "node.network_probe"
            grant = call("Grant.Issue", {
                "principal_id": "local-owner", "actions": ["task.submit", "task.read", tool],
                "tool_targets": [ref["resource_id"] for ref in references.values()],
                "ttl_seconds": 120, "units": len(references),
            })
            delegation = grant["delegation_ref"]["resource_id"]
            for name, ref in references.items():
                listener, endpoint = listeners[name]
                task = ctl("submit", tool, "--delegation", delegation, "--key", name, params=target(ref))
                done = ctl("wait", task["task_id"])
                assert done["lifecycle"] == "Succeeded", done
                artifact, = done["result"]["artifact_refs"]
                observation = call("Artifact.Read", {"reference": artifact})["observation"]
                assert observation["endpoint"] == endpoint and observation["tcp"]["status"] == "Connected", observation
                stream, _ = listener.accept()
                with stream:
                    stream.settimeout(4)
                    assert stream.recv(1) == b""
        finally:
            config.write_bytes(original)
            assert restart() == original_identity
    print("PASS the installed service connected to both scoped IPv4 and IPv6 endpoints")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("--service", default="psiosd.service")
    parser.add_argument("--user", default="psios")
    parser.add_argument("--client", type=Path, default=Path("/usr/bin/psiosctl"))
    arguments = parser.parse_args()
    verify(arguments.config, arguments.service, arguments.user, arguments.client)
