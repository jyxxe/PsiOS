"""Register exact process inspection without extending a format 52 grant."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess

from support import Failure, Node
from test_process_observation import TOOL, issue, prepare


def main(previous):
    node = Node(previous)
    try:
        scope = node.call("Node.Get", {})["reference"]["resource_id"]
        agent, list_grant = node.agent(["task.submit", "task.read", "node.processes"])
        token = agent["token"]
        pid = os.getpid()
        request = node.prepare("node.processes", {"after_pid": pid - 1, "limit": 1}, token=token,
                               delegation=list_grant["delegation_ref"]["resource_id"])
        original = node.wait(node.call("Task.Submit", request, token)["task_id"])
        assert original["lifecycle"] == "Succeeded", original
        original_ref, = original["result"]["artifact_refs"]
        original_read = node.call("Artifact.Read", {"reference": original_ref}, token)
        assert original_read["items"][0]["pid"] == pid and "process_ref" not in original_read["items"][0]
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 52 and before["migration_required"], before
        node.start()
        assert node.call("Artifact.Read", {"reference": original_ref}, token) == original_read
        newer = node.wait(node.submit("node.processes", {"after_pid": pid - 1, "limit": 1}))
        page = node.call("Artifact.Read", {"reference": newer["result"]["artifact_refs"][0]})
        process = page["items"][0]["process_ref"]
        assert process["node_id"] == scope and process["pid"] == pid
        assert process["start_ticks"] == original_read["items"][0]["start_ticks"]
        try:
            prepare(node, process, list_grant, token)
        except Failure as denied:
            assert denied.code == "PermissionDenied", denied
        else:
            raise AssertionError("the original listing grant acquired process inspection")
        new_grant = issue(node, agent["principal_id"], [scope])
        result = node.call("Task.Submit", prepare(node, process, new_grant, token), token)
        task = node.wait(result["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        observed = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)
        assert observed["process_ref"] == process and int(observed["cpu"]["ticks_per_second"]) > 0
        assert node.get(f"tool-{TOOL}")["resource"]["spec"]["effect_class"] == "ReadOnly"
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 53 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 52 listing grant and retained evidence stayed intact while exact inspection required a new grant")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
