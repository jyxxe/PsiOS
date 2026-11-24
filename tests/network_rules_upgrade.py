"""Migrate an actual format 7 node, retaining grant scope and existing route cursors."""
import argparse
import json
from pathlib import Path
import subprocess

from support import Failure, Node


def observe(n, tool, params, delegation):
    request = n.prepare(tool, params, delegation=delegation)
    task = n.wait(n.call("Task.Submit", request)["task_id"])
    assert task["lifecycle"] == "Succeeded", task
    reference, = task["result"]["artifact_refs"]
    return reference, n.call("Artifact.Read", {"reference": reference})


def upgrade(previous, current):
    n = Node(previous)
    try:
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        owner = n.get("owner-delegation")["reference"]
        grant = n.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=[node],
                       actions=["task.submit", "task.read", "node.routes"], units=4, ttl_seconds=120))
        delegation = grant["delegation_ref"]["resource_id"]
        prior_ref, prior = observe(n, "node.routes", {"family": "inet", "limit": 1}, delegation)
        assert prior["next_cursor"], prior
        try:
            n.prepare("node.routing_rules", {})
        except Failure as error:
            assert error.code == "UnsupportedCapability", error
        else:
            raise AssertionError("previous binary unexpectedly registered policy rules")
        n.stop()
        new = str((current / "psiosd").resolve())
        report = json.loads(subprocess.check_output([new, "--config", n.config, "verify"]))
        assert report["database_format"] == 7 and report["migration_required"], report
        n.daemon[0] = new
        n.start()
        assert n.call("Node.Get", {})["reference"]["resource_id"] == node
        assert n.get("owner-delegation")["reference"] == owner
        assert n.get(delegation)["reference"] == grant["delegation_ref"]
        assert n.call("Artifact.Read", {"reference": prior_ref}) == prior
        _, continued = observe(n, "node.routes", {"family": "inet", "cursor": prior["next_cursor"]}, delegation)
        assert continued["snapshot_digest"] == prior["snapshot_digest"] and continued["routes"], continued
        for scope in ("owner-delegation", delegation):
            try:
                n.prepare("node.routing_rules", {}, delegation=scope)
            except Failure as error:
                assert error.code == "PermissionDenied", error
            else:
                raise AssertionError("migration widened an existing grant")
        rules_grant = n.call("Grant.Issue", dict(principal_id="local-owner", tool_targets=[node],
                             actions=["task.submit", "task.read", "node.routing_rules"], units=1, ttl_seconds=120))
        rules_ref, rules = observe(n, "node.routing_rules", {"family": "inet"},
                                   rules_grant["delegation_ref"]["resource_id"])
        assert any(r["table"] == 255 and r["action"] == 1 for r in rules["rules"]), rules
        n.stop()
        report = json.loads(subprocess.check_output([new, "--config", n.config, "verify"]))
        assert report["database_format"] >= 8 and not report["migration_required"], report
        old = subprocess.run([previous.resolve() / "psiosd", "--config", n.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        n.start()
        assert n.call("Artifact.Read", {"reference": rules_ref}) == rules
        print("PASS format 7 routing-rule catalog migration, retained route cursor, scoped authority and old-writer refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
