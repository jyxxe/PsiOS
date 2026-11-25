"""Observe the same Linux process through authorised, durable Core Tasks."""
import os
from pathlib import Path
import select
import subprocess
import sys
import unittest

from support import Failure, Node, target


TOOL = "node.process_inspect"
COMM = "psi ) odd\n"
CHILD = """
import ctypes, sys, time
assert ctypes.CDLL(None).prctl(15, b"psi ) odd\\n", 0, 0, 0) == 0
print("ready", flush=True)
while sys.stdin.buffer.read(1):
    end = time.process_time_ns() + 50_000_000
    while time.process_time_ns() < end:
        pass
    print("worked", flush=True)
"""


def issue(node, principal, targets=None):
    grant = {"principal_id": principal, "actions": ["task.submit", "task.read", TOOL],
             "units": 10, "ttl_seconds": 120}
    if targets is not None:
        grant["tool_targets"] = targets
    return node.call("Grant.Issue", grant)


def prepare(node, process, grant, token=None):
    return node.prepare(TOOL, process, token=token, delegation=grant["delegation_ref"]["resource_id"])


def read_line(child):
    assert select.select([child.stdout], [], [], 10)[0], "child did not report progress"
    return child.stdout.readline().strip()


class ProcessObservationTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.scope = self.node.call("Node.Get", {})["reference"]["resource_id"]

    def denied(self, code, action, *args):
        with self.assertRaises(Failure) as failure:
            action(*args)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def observe(self, identity, grant, token=None):
        n = self.node
        submitted = n.call("Task.Submit", prepare(n, identity, grant, token), token)
        task = n.wait(submitted["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        reference, = task["result"]["artifact_refs"]
        return reference, n.call("Artifact.Read", {"reference": reference}, token)

    def test_same_process_usage_exit_and_live_scope(self):
        n = self.node
        with subprocess.Popen([sys.executable, "-c", CHILD], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE) as child:
            self.assertEqual(read_line(child), b"ready")
            page = n.perform("node.processes", {"after_pid": child.pid - 1, "limit": 1})
            brief, = page["items"]
            ref = brief["process_ref"]
            self.assertEqual(brief["name"], COMM)
            self.assertEqual(ref, {"node_id": self.scope, "pid": child.pid, "boot_id": page["boot_id"],
                                   "proc_view_id": str(Path("/proc").stat().st_dev),
                                   "start_ticks": brief["start_ticks"]})
            capability = n.get(f"tool-{TOOL}")
            self.assertEqual(capability["resource"]["spec"]["effect_class"], "ReadOnly")
            self.denied("PermissionDenied", n.prepare, TOOL, ref)
            self.denied("InvalidArgument", issue, n, "local-owner")

            agent, list_access = n.agent(["task.submit", "task.read", "node.processes"])
            token = agent["token"]
            self.denied("PermissionDenied", prepare, n, ref, list_access, token)
            wrong = issue(n, agent["principal_id"], ["profile-local"])
            self.denied("PermissionDenied", n.call, "Task.Submit", prepare(n, ref, wrong, token), token)
            allowed = issue(n, agent["principal_id"], [self.scope])
            first_ref, start = self.observe(ref, allowed, token)
            self.assertEqual(start["process_ref"], ref)
            self.assertEqual(start["name"], COMM)
            self.assertEqual(start["parent_pid"], os.getpid())
            self.assertEqual(start["threads"], 1)
            self.assertEqual(start["memory"]["page_bytes"], os.sysconf("SC_PAGE_SIZE"))
            self.assertEqual(start["cpu"]["ticks_per_second"], os.sysconf("SC_CLK_TCK"))

            child.stdin.write(b"x")
            child.stdin.flush()
            self.assertEqual(read_line(child), b"worked")
            n.stop()
            n.start()
            self.assertEqual(n.call("Artifact.Read", {"reference": first_ref}, token), start)
            _, later = self.observe(ref, allowed, token)
            total = lambda value: int(value["cpu"]["user_ticks"]) + int(value["cpu"]["system_ticks"])
            self.assertGreater(total(later), total(start))
            self.assertGreater(int(later["memory"]["virtual_bytes"]), 0)

            for stale in ({**ref, "start_ticks": str(int(ref["start_ticks"]) + 1)},
                          {**ref, "node_id": "other-node"},
                          {**ref, "boot_id": "00000000-0000-0000-0000-000000000000"},
                          {**ref, "proc_view_id": str(int(ref["proc_view_id"]) + 1)}):
                request = prepare(n, stale, allowed, token)
                task = n.wait(n.call("Task.Submit", request, token)["task_id"])
                self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]),
                                 ("Failed", "PreconditionFailed"), task)
            child.stdin.close()
            self.assertEqual(child.wait(10), 0)
            task = n.wait(n.call("Task.Submit", prepare(n, ref, allowed, token), token)["task_id"])
            self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]), ("Failed", "NotFound"), task)
            n.call("Grant.Revoke", target(allowed["grant_ref"]))
            self.denied("PermissionDenied", prepare, n, ref, allowed, token)
