"""Check the actual format 29 authority's inline bytes across migration."""
import argparse
import hashlib
import json
from pathlib import Path
import signal
import subprocess

from support import Node


def main(previous):
    n = Node(previous)
    try:
        request = n.prepare("system.status", {}, "published-by-format-29")
        task_id = n.call("Task.Submit", request)["task_id"]
        result = n.wait(task_id)["result"]
        reference, = result["artifact_refs"]
        params = {"reference": reference}
        body = n.call("Artifact.Read", params)
        page = n.call("Artifact.ReadChunk", dict(params, offset=0, length=37))
        ref_json = json.dumps(reference).encode()
        legacy = subprocess.check_output([n.binaries / "psiosctl", "--config", n.config, "artifact"],
                                         input=ref_json, timeout=15)
        assert "sha256:" + hashlib.sha256(legacy).hexdigest() == reference["digest"]
        n.stop(signal.SIGKILL)

        current = Path("target/debug").absolute()
        n.daemon[0] = str(current / "psiosd")
        before = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 29 and before["migration_required"], before
        n.start()
        assert n.call("Task.Get", {"id": task_id})["result"] == result
        assert n.call("Task.Submit", request)["deduplicated"]
        assert n.call("Artifact.Read", params) == body
        assert n.call("Artifact.ReadChunk", dict(params, offset=0, length=37)) == page
        resolved = n.call("Resource.Resolve", params)
        assert resolved["value"] == body and resolved["reference"] == reference
        assert subprocess.check_output([current / "psiosctl", "--config", n.config, "artifact"],
                                       input=ref_json, timeout=15) == legacy
        fresh = n.wait(n.submit("system.status", {}))["result"]["artifact_refs"][0]
        assert n.get(fresh["resource_id"])["resource"]["spec"]["content_layout"] == "ChunkedJsonV1"
        n.stop()
        after = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert after["node_id"] == before["node_id"]
        assert after["database_format"] >= 30 and not after["migration_required"], after
        old = subprocess.run([str(n.binaries / "psiosd"), "--config", n.config, "serve"],
                             capture_output=True, timeout=15)
        assert old.returncode != 0 and b"SchemaVersionUnsupported" in old.stderr, old
        print("PASS format 29 artifact bytes, reference, Task replay, new publication and downgrade refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
