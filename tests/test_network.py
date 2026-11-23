"""Local kernel link/address observations through a scoped Task, including live paging."""
import json
import os
import subprocess
import unittest

from support import Failure, Node


class NetworkTests(unittest.TestCase):
    def test_interface_task_matches_kernel_and_does_not_widen_existing_grants(self):
        n = Node("target/debug")
        self.addCleanup(n.close)
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        for token, delegation in [(n.owner, "owner-delegation"),
                                   self.actor(n, ["task.submit", "task.read", "node.inventory"], node)]:
            with self.assertRaises(Failure) as context:
                n.prepare("node.interfaces", {}, token=token, delegation=delegation)
            self.assertEqual(context.exception.code, "PermissionDenied")
        wrong, scope = self.actor(n, ["task.submit", "task.read", "node.interfaces"], "profile-local")
        with self.assertRaises(Failure) as context:
            prepared = n.prepare("node.interfaces", {}, token=wrong, delegation=scope)
            n.call("Task.Submit", prepared, wrong)
        self.assertEqual(context.exception.code, "PermissionDenied")
        token, grant = self.actor(n, ["task.submit", "task.read", "node.interfaces"], node)

        def page(params):
            submit = n.prepare("node.interfaces", params, token=token, delegation=grant)
            receipt = n.call("Task.Submit", submit, token)
            result = n.wait(receipt["task_id"])
            self.assertEqual(result["lifecycle"], "Succeeded", result)
            return n.call("Artifact.Read", {"reference": result["result"]["artifact_refs"][0]}, token)

        with self.assertRaises(Failure) as context:
            page({"limit": 0})
        self.assertEqual(context.exception.code, "InvalidArgument")
        rows = []
        after = 0
        while True:
            data = page({"after_ifindex": after, "limit": 1})
            self.assertEqual(data["network_namespace_inode"], str(os.stat("/proc/self/ns/net").st_ino))
            self.assertEqual(len(data["interfaces"]), 1)
            row = data["interfaces"][0]
            self.assertGreater(row["ifindex"], after)
            rows.append(row)
            after = data["next_after_ifindex"]
            if after is None:
                break
            self.assertEqual(after, row["ifindex"])
        live = json.loads(subprocess.check_output(["/usr/sbin/ip", "-j", "address", "show"]))
        self.assertEqual({i["ifindex"] for i in rows}, {i["ifindex"] for i in live})
        links = {i["ifname"]: i for i in live}
        for row in rows:
            raw = links[row["ifname"]]
            self.assertEqual(row["mtu"], raw["mtu"])
            self.assertEqual(row["operstate"], raw["operstate"])
            self.assertEqual([(a["local"], a["prefixlen"], a.get("peer")) for a in row["addr_info"]],
                             [(a["local"], a["prefixlen"], a.get("address")) for a in raw["addr_info"]])
        self.assertEqual(page({"after_ifindex": 4294967295})["interfaces"], [])

    def actor(self, n, actions, target):
        a = n.call("Agent.Register", {"name": "observer"})
        grant = n.call("Grant.Issue", {"principal_id": a["principal_id"], "actions": actions,
                       "tool_targets": [target], "ttl_seconds": 600, "units": 1000})
        return a["token"], grant["delegation_ref"]["resource_id"]
