"""Add opaque uploads while retaining the original JSON result and authority."""
import argparse
import base64
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node
from test_artifact_ingress import chunk, upload_request


def main(previous):
    node = Node(previous)
    try:
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        old_task = node.wait(node.submit("system.status", {}))
        old_ref, = old_task["result"]["artifact_refs"]
        existing = node.call("Artifact.Read", {"reference": old_ref})
        agent, _ = node.agent(["task.read", "resource.read"])
        token = agent["token"]
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 53 and before["migration_required"], before
        node.start()
        assert node.call("Artifact.Read", {"reference": old_ref}) == existing
        payload = b"\xff\x00uninterpreted upload\n"
        request = upload_request(payload)
        try:
            node.call("Artifact.Stage", request, token)
        except Failure as denied:
            assert denied.code == "PermissionDenied", denied
        else:
            raise AssertionError("the old result-reading grant acquired artifact upload")
        node.call("Grant.Issue", {"principal_id": agent["principal_id"], "units": 2, "ttl_seconds": 120,
                                 "actions": ["artifact.upload", "artifact.read"], "tool_targets": [node_id]})
        stage = node.call("Artifact.Stage", request, token)
        node.call("Artifact.Stage.PutChunk", chunk(stage, 0, payload), token)
        receipt = node.call("Artifact.Publish", {"stage_id": stage["stage_id"]}, token)
        data = node.call("Artifact.ReadChunk", {"reference": receipt["artifact_ref"], "offset": 0, "length": 32}, token)
        assert base64.b64decode(data["data_base64"]) == payload
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 54 and not after["migration_required"], after
        assert before["node_id"] == after["node_id"] == node_id
        print("PASS format 53 results and credentials survived while opaque upload required an explicit grant")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
