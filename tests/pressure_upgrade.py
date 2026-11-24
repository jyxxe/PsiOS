"""Upgrade a format-21 authority and check that pressure needs a new grant."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node
from test_pressure import TOOL, artifact, issue, submit


def exercise(previous, current):
    node = Node(previous)
    try:
        agent, grant = node.agent(["system.status", "task.submit", "task.read"])
        token = agent["token"]
        delegation = grant["delegation_ref"]["resource_id"]
        request = node.prepare("system.status", {}, token=token, delegation=delegation)
        task = node.call("Task.Submit", request, token)["task_id"]
        prior = node.wait(task)
        assert prior["lifecycle"] == "Succeeded", prior
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"], timeout=15))
        assert report["database_format"] == 21 and report["migration_required"], report
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Task.Get", {"id": task}, token) == prior
        assert node.get(f"tool-{TOOL}")["value"]["capability_id"] == TOOL
        try:
            submit(node, grant, token)
        except Failure as error:
            assert error.code == "PermissionDenied", error
        else:
            raise AssertionError("the older grant unexpectedly authorises pressure observations")
        scope = node.call("Node.Get", {})["reference"]["resource_id"]
        new = issue(node, agent["principal_id"], scope)
        _, result = artifact(node, submit(node, new, token), token)
        assert result["node_ref"]["resource_id"] == scope, result

        node.stop()
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"], timeout=15))
        assert report["database_format"] >= 22 and not report["migration_required"], report
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format-21 migration, prior result, scoped pressure grant and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
