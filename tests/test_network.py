"""Local link, address and route observations through scoped Tasks."""
from collections import Counter
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import unittest

from support import Failure, Node


class NetworkTests(unittest.TestCase):
    @unittest.skipUnless(os.getenv("PSIOS_TEST_BULK_NETWORK"), "requires a network namespace with thousands of scoped addresses")
    def test_large_real_interface_result_can_be_exported(self):
        n = Node("target/debug")
        self.addCleanup(n.close)
        self.assertFalse(Path("/usr/sbin/ip").exists())
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        grant = n.call("Grant.Issue", dict(principal_id="local-owner", units=1, ttl_seconds=120,
                       actions=["task.submit", "task.read", "node.interfaces"], tool_targets=[node]))
        request = n.prepare("node.interfaces", {}, delegation=grant["delegation_ref"]["resource_id"])
        task = n.wait(n.call("Task.Submit", request)["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        ref, = task["result"]["artifact_refs"]
        with self.assertRaises(Failure) as limited:
            n.call("Artifact.Read", {"reference": ref})
        self.assertEqual(limited.exception.code, "QuotaExceeded")
        n.stop()
        n.start()
        data = subprocess.check_output([n.binaries / "psiosctl", "--config", n.config, "artifact"],
                                       input=json.dumps(ref).encode(), timeout=30)
        self.assertGreater(len(data), 512 * 1024)
        self.assertEqual("sha256:" + hashlib.sha256(data).hexdigest(), ref["digest"])
        loopback, = json.loads(data)["interfaces"]
        self.assertEqual(loopback["ifname"], "lo")
        self.assertEqual(sum(a["local"].startswith("2001:db8:abcd:") for a in loopback["addr_info"]), 8000)

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
        reference = os.getenv("PSIOS_TEST_NETWORK_REFERENCE")
        if reference:
            self.assertFalse(Path("/usr/sbin/ip").exists())
            live = json.loads(Path(reference).read_bytes())
        else:
            live = json.loads(subprocess.check_output(["/usr/sbin/ip", "-j", "address", "show"]))
        self.assertEqual({i["ifindex"] for i in rows}, {i["ifindex"] for i in live})
        links = {i["ifname"]: i for i in live}
        for row in rows:
            raw = links[row["ifname"]]
            self.assertEqual(row["mtu"], raw["mtu"])
            self.assertEqual(row["link_type"], raw["link_type"])
            self.assertEqual(set(row["flags"]), set(raw["flags"]))
            self.assertEqual([a["scope"] for a in row["addr_info"]], [a["scope"] for a in raw["addr_info"]])
            self.assertEqual(row["operstate"], raw["operstate"])
            self.assertEqual([(a["local"], a["prefixlen"], a.get("peer")) for a in row["addr_info"]],
                             [(a["local"], a["prefixlen"], a.get("address")) for a in raw["addr_info"]])
        self.assertEqual(page({"after_ifindex": 4294967295})["interfaces"], [])

    def test_route_tasks_match_kernel_and_scope_continuations(self):
        n = Node("target/debug")
        self.addCleanup(n.close)
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        actions = ["task.submit", "task.read", "node.routes"]
        for actor, scope in [(n.owner, "owner-delegation"),
                             self.actor(n, ["task.submit", "task.read", "node.interfaces"], node)]:
            with self.assertRaises(Failure) as context:
                n.prepare("node.routes", {}, token=actor, delegation=scope)
            self.assertEqual(context.exception.code, "PermissionDenied")
        wrong, scope = self.actor(n, actions, "profile-local")
        with self.assertRaises(Failure) as context:
            prepared = n.prepare("node.routes", {}, token=wrong, delegation=scope)
            n.call("Task.Submit", prepared, wrong)
        self.assertEqual(context.exception.code, "PermissionDenied")
        token, grant = self.actor(n, actions, node)
        other, other_grant = self.actor(n, actions, node)

        def submit(params, actor=token, delegation=grant):
            prepared = n.prepare("node.routes", params, token=actor, delegation=delegation)
            receipt = n.call("Task.Submit", prepared, actor)
            return n.wait(receipt["task_id"])

        def read(task, actor=token):
            self.assertEqual(task["lifecycle"], "Succeeded", task)
            return n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, actor)

        def rejected(params, code, actor=token, delegation=grant):
            task = submit(params, actor, delegation)
            self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]), ("Failed", code), task)

        for invalid in ({"limit": 0}, {"limit": 129}, {"family": "inet4"}, {"table": 0},
                        {"cursor": {"offset": 1, "snapshot_digest": "invalid"}}):
            with self.assertRaises(Failure) as context:
                submit(invalid)
            self.assertEqual(context.exception.code, "InvalidArgument")

        boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        namespace = str(os.stat("/proc/self/ns/net").st_ino)
        first = submit({"limit": 1})
        page = read(first)
        with self.assertRaises(Failure) as context:
            n.call("Artifact.Read", {"reference": first["result"]["artifact_refs"][0]}, other)
        self.assertEqual(context.exception.code, "PermissionDenied")
        n.stop()
        n.start()
        self.assertEqual(read(first), page)

        rows = []
        initial_cursor = page["next_cursor"]
        digest = page["snapshot_digest"]
        while True:
            self.assertEqual((page["boot_id"], page["network_namespace_inode"]), (boot, namespace))
            self.assertEqual(page["snapshot_digest"], digest)
            rows.extend(page["routes"])
            cursor = page["next_cursor"]
            if cursor is None:
                break
            self.assertEqual(cursor["offset"], len(rows))
            page = read(submit({"cursor": cursor, "limit": 4}))

        reference = os.getenv("PSIOS_TEST_ROUTE_REFERENCE")
        if reference:
            self.assertFalse(Path("/usr/sbin/ip").exists())
            live = json.loads(Path(reference).read_bytes())
        else:
            live = [json.loads(subprocess.check_output(["/usr/sbin/ip", "-j", "-N", *args])) for args in
                    (["link", "show"], ["-4", "route", "show", "table", "all"],
                     ["-6", "route", "show", "table", "all"])]
        ifindices = {row["ifname"]: row["ifindex"] for row in live[0]}

        def prefix(value):
            if value is None:
                return None
            return str(ipaddress.ip_network(f'{value["address"]}/{value["length"]}'))

        def ip_prefix(value, family):
            if value == "default":
                return "0.0.0.0/0" if family == "inet" else "::/0"
            return str(ipaddress.ip_network(value)) if value is not None else None

        def gateway(entry):
            return entry.get("gateway", entry.get("via", {}).get("host"))

        expected = Counter(
            (family, int(row.get("table", 254)), ip_prefix(row["dst"], family),
             ip_prefix(row.get("from"), family), int(row.get("type", 1)), row.get("metric"),
             gateway(row), ifindices.get(row.get("dev")), row.get("prefsrc"), row.get("nhid"),
             tuple((ifindices[hop["dev"]], hop.get("weight", 1), gateway(hop)) for hop in row.get("nexthops", [])))
            for family, table in zip(("inet", "inet6"), live[1:]) for row in table
        )
        observed = Counter(
            (row["family"], row["table"], prefix(row["destination"]), prefix(row["source"]), row["kind"],
             row["metric"], row["gateway"], row["output_ifindex"], row["preferred_source"], row["nexthop_id"],
             tuple((hop["ifindex"], hop["weight"], hop["gateway"]) for hop in row["multipath"])) for row in rows
        )
        self.assertEqual(observed, expected)
        self.assertEqual(read(submit({"family": "inet6", "table": 1000}))["routes"],
                         [row for row in rows if row["family"] == "inet6" and row["table"] == 1000])
        if initial_cursor is not None:
            rejected({"cursor": initial_cursor, "family": "inet"}, "ResyncRequired")
            rejected({"cursor": initial_cursor}, "ResyncRequired", other, other_grant)
            rejected({"cursor": dict(initial_cursor, offset=len(rows))}, "InvalidArgument")
        if os.getenv("PSIOS_TEST_MUTATE_ROUTES"):
            self.assertIsNotNone(initial_cursor)
            subprocess.run(["/usr/sbin/ip", "-4", "route", "add", "blackhole", "198.19.0.0/16", "table", "1000"],
                           check=True)
            try:
                rejected({"cursor": initial_cursor}, "ResyncRequired")
                fresh = read(submit({"table": 1000, "family": "inet"}))
                self.assertTrue(any(prefix(row["destination"]) == "198.19.0.0/16" and row["kind"] == 6
                                    for row in fresh["routes"]))
                self.assertEqual(read(first)["snapshot_digest"], digest)
            finally:
                subprocess.run(["/usr/sbin/ip", "-4", "route", "del", "blackhole", "198.19.0.0/16", "table", "1000"],
                               check=True)

    def actor(self, n, actions, target):
        a = n.call("Agent.Register", {"name": "observer"})
        grant = n.call("Grant.Issue", {"principal_id": a["principal_id"], "actions": actions,
                       "tool_targets": [target], "ttl_seconds": 600, "units": 1000})
        return a["token"], grant["delegation_ref"]["resource_id"]
