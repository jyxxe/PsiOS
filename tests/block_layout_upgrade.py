"""Keep format-22 device grants scoped after installing the block layout tool."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node
from test_block_layout import TOOL, issue, receipt, submit, whole_disk


def exercise(previous, current):
    node = Node(previous)
    try:
        disk = whole_disk()
        assert disk is not None, "the migration fixture requires a real sysfs disk"
        scope = node.call("Node.Get", {})["reference"]["resource_id"]
        agent = node.call("Agent.Register", {"name": "legacy-device-observer"})
        token = agent["token"]
        grant = node.call("Grant.Issue", dict(principal_id=agent["principal_id"], tool_targets=[scope],
            actions=["node.devices", "task.submit", "task.read"], units=4, ttl_seconds=180))
        delegation = grant["delegation_ref"]["resource_id"]
        first = node.prepare("node.devices", {}, token=token, delegation=delegation)
        task = node.call("Task.Submit", first, token)["task_id"]
        original = receipt(node, task, token)
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"], timeout=15))
        assert report["database_format"] == 22 and report["migration_required"], report
        node.daemon[0] = str(binary)
        node.start()
        assert receipt(node, task, token) == original
        assert node.get(f"tool-{TOOL}")["value"]["capability_id"] == TOOL
        try:
            submit(node, grant, disk.name, token=token)
        except Failure as error:
            assert error.code == "PermissionDenied", error
        else:
            raise AssertionError("device inventory grant unexpectedly widened to layouts")
        access = issue(node, agent["principal_id"], scope)
        layout = receipt(node, submit(node, access, disk.name, token=token), token)
        assert layout["layout"]["major_minor"] == disk.name and layout["node_ref"]["resource_id"] == scope, layout

        node.stop()
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"], timeout=15))
        assert report["database_format"] >= 23 and not report["migration_required"], report
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format-22 migration, prior device receipt, new layout grant and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
