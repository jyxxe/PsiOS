"""Exercise live resolver observations against a file in a private mount namespace."""
import argparse
import json
import os
from pathlib import Path
import subprocess

from support import Failure, Node, target


TOOL = "node.resolver"
RESOLVER = Path("/etc/resolv.conf")


def denied(operation, code):
    try:
        operation()
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"expected {code}")


def exercise(args, node):
    if args.previous_bin_dir:
        before_task = node.submit("system.status", {})
        before = node.wait(before_task)
        assert before["lifecycle"] == "Succeeded", before
        denied(lambda: node.get(f"tool-{TOOL}"), "NotFound")
        node.stop()
        baseline = json.loads(subprocess.check_output([*node.daemon, "verify", "--scratch-dir", node.base]))
        assert baseline["database_format"] == 148, baseline
        node.binaries = args.bin_dir
        node.daemon[0] = str(args.bin_dir / "psiosd")
        node.start()
        assert node.call("Task.Get", {"id": before_task}) == before

    capability = node.get(f"tool-{TOOL}")["resource"]["spec"]
    assert capability["effect_class"] == "ReadOnly" and capability["requires_approval"] is False, capability
    authority = node.call("Node.Get", {})["reference"]["resource_id"]
    denied(lambda: node.prepare(TOOL, {}), "PermissionDenied")
    actor, old_grant = node.agent(["task.submit", "task.read", "node.interfaces"])
    token = actor["token"]
    denied(lambda: node.prepare(TOOL, {}, token=token, delegation=old_grant["delegation_ref"]["resource_id"]),
           "PermissionDenied")
    access = dict(principal_id=actor["principal_id"], actions=["task.submit", "task.read", TOOL],
                  units=12, ttl_seconds=180)
    other = node.call("Grant.Issue", {**access, "tool_targets": ["profile-local"]})
    denied(lambda: node.call("Task.Submit", node.prepare(TOOL, {}, token=token,
           delegation=other["delegation_ref"]["resource_id"]), token), "PermissionDenied")
    grant = node.call("Grant.Issue", {**access, "tool_targets": [authority]})
    delegation = grant["delegation_ref"]["resource_id"]
    denied(lambda: node.call("Task.Submit", node.prepare(TOOL, {"path": "/etc/passwd"},
           token=token, delegation=delegation), token), "InvalidArgument")

    def observe(error=None):
        prepared = node.prepare(TOOL, {}, token=token, delegation=delegation)
        task = node.wait(node.call("Task.Submit", prepared, token)["task_id"])
        if error:
            assert task["lifecycle"] == "Failed" and task["conditions"][0]["type"] == error, task
            return
        assert task["lifecycle"] == "Succeeded", task
        reference, = task["result"]["artifact_refs"]
        return reference, node.call("Artifact.Read", {"reference": reference}, token)

    args.fixture.write_text("# comment\nsearch private.invalid\nnameserver 192.0.2.53\n"
                            "nameserver fe80::53%eth0\nnameserver absent.invalid\n"
                            "nameserver 192.0.2.53 # repeat\n")
    reference, initial = observe()
    assert initial["path"] == str(RESOLVER)
    assert initial["network_namespace_inode"] == str(os.stat("/proc/self/ns/net").st_ino)
    assert initial["mount_namespace_inode"] == str(os.stat("/proc/self/ns/mnt").st_ino)
    assert initial["nameservers"] == [{"address": "192.0.2.53"}, {"address": "fe80::53", "zone": "eth0"},
                                      {"address": "192.0.2.53"}]
    assert initial["invalid_nameserver_directives"] == 1 and "private.invalid" not in json.dumps(initial)

    args.fixture.write_text("nameserver 2001:db8::53\n")
    node.stop()
    node.start()
    second, updated = observe()
    assert second != reference and updated["nameservers"] == [{"address": "2001:db8::53"}]
    assert node.call("Artifact.Read", {"reference": reference}, token) == initial

    args.fixture.chmod(0o666)
    try:
        observe("PermissionDenied")
    finally:
        args.fixture.chmod(0o600)
    args.fixture.write_bytes(b"#" * (64 * 1024 + 1))
    observe("QuotaExceeded")
    args.fixture.write_text("nameserver ::1\n" * 33)
    observe("QuotaExceeded")
    node.call("Grant.Revoke", target(grant["grant_ref"]))
    denied(lambda: node.prepare(TOOL, {}, token=token, delegation=delegation), "PermissionDenied")
    if args.previous_bin_dir:
        node.stop()
        final = json.loads(subprocess.check_output([*node.daemon, "verify", "--scratch-dir", node.base]))
        assert final["database_format"] == 149 and final["witness"] == "Confirmed", final
    print("Verified real resolver readings, namespace identity, scoped Task grants, bounds and persistent observations.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--previous-bin-dir", type=Path)
    args = parser.parse_args()
    if args.fixture.resolve() == RESOLVER.resolve() or not args.fixture.samefile(RESOLVER):
        parser.error("bind a separate disposable fixture over /etc/resolv.conf in a private mount namespace")
    args.bin_dir = args.bin_dir.resolve()
    node = Node(args.previous_bin_dir or args.bin_dir)
    try:
        exercise(args, node)
    finally:
        node.close()
