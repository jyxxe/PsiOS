"""Upgrade a live format-20 authority and verify the probe grant boundary."""
import argparse
import json
from pathlib import Path
import signal
import socket
import subprocess

from support import Failure, Node, target
from test_network_probe import TOOL


def rejected(action):
    try:
        action()
    except Failure as error:
        assert error.code == "PermissionDenied", error
    else:
        raise AssertionError("upgrade supplied an unissued network probe grant")


def upgrade(previous, current):
    node = Node(previous)
    try:
        actor, grant = node.agent(["system.status", "task.submit", "task.read"])
        token = actor["token"]
        old = node.prepare("system.status", {}, token=token, delegation=grant["delegation_ref"]["resource_id"])
        task = node.call("Task.Submit", old, token)["task_id"]
        receipt = node.wait(task)
        assert receipt["lifecycle"] == "Succeeded"
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        state = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert state["database_format"] == 20 and state["migration_required"], state
        node.daemon[0] = str(binary)
        with socket.create_server(("127.0.0.1", 0)) as listener:
            listener.settimeout(5)
            config = json.loads(node.config.read_text())
            config["network_probes"] = [dict(name="new", endpoint=f"127.0.0.1:{listener.getsockname()[1]}")]
            node.config.write_text(json.dumps(config))
            node.start()
            assert node.call("Task.Get", {"id": task}, token) == receipt
            assert node.get(f"tool-{TOOL}")["value"]["capability_id"] == TOOL
            reference, = node.call("Resource.List", {"kind": "NetworkProbe"})["items"]
            request = {"id": reference["resource_id"]}
            rejected(lambda: node.call("NetworkProbe.Get", request, token))
            rejected(lambda: node.call("NetworkProbe.Get", request))
            issued = node.call("Grant.Issue", dict(principal_id=actor["principal_id"],
                actions=[TOOL, "task.submit", "task.read"], tool_targets=[request["id"]], units=1, ttl_seconds=60))
            probe = node.prepare(TOOL, target(reference), token=token,
                                 delegation=issued["delegation_ref"]["resource_id"])
            checked = node.wait(node.call("Task.Submit", probe, token)["task_id"])
            assert checked["lifecycle"] == "Succeeded", checked
            evidence = node.call("Artifact.Read", {"reference": checked["result"]["artifact_refs"][0]}, token)
            assert evidence["observation"]["status"] == "Connected", evidence
            connection, _ = listener.accept()
            with connection:
                assert connection.recv(1) == b""

        node.stop()
        state = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert state["database_format"] >= 21 and not state["migration_required"], state
        config.pop("network_probes")
        node.config.write_text(json.dumps(config))
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode != 0 and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format 20 migration, preserved task, unexpanded grants, scoped TCP check and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
