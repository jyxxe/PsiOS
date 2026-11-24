"""Local links, addresses, routes and policy rules through scoped Tasks."""
from collections import Counter
import errno
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import socket
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

    def test_policy_rules_preserve_kernel_order_and_scope_continuations(self):
        n = Node("target/debug")
        self.addCleanup(n.close)
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        tool = "node.routing_rules"
        actions = ["task.submit", "task.read", tool]
        route_token, route_grant = self.actor(n, ["task.submit", "task.read", "node.routes"], node)
        for actor, grant in [(n.owner, "owner-delegation"), (route_token, route_grant)]:
            with self.assertRaises(Failure) as denied:
                n.prepare(tool, {}, token=actor, delegation=grant)
            self.assertEqual(denied.exception.code, "PermissionDenied")
        wrong, wrong_grant = self.actor(n, actions, "profile-local")
        with self.assertRaises(Failure) as denied:
            n.call("Task.Submit", n.prepare(tool, {}, token=wrong, delegation=wrong_grant), wrong)
        self.assertEqual(denied.exception.code, "PermissionDenied")
        token, grant = self.actor(n, actions, node)
        principal = n.call("Principal.WhoAmI", {}, token)["principal_id"]
        alternate = n.call("Grant.Issue", dict(principal_id=principal, actions=actions, tool_targets=[node],
                                              ttl_seconds=120, units=5))["delegation_ref"]["resource_id"]

        def submit(params, delegation=grant):
            req = n.prepare(tool, params, token=token, delegation=delegation)
            return n.wait(n.call("Task.Submit", req, token)["task_id"])

        def read(task):
            self.assertEqual(task["lifecycle"], "Succeeded", task)
            return n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)

        def rejected(params, code, delegation=grant):
            task = submit(params, delegation)
            self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]), ("Failed", code), task)

        for invalid in ({"limit": 0}, {"limit": 129}, {"family": "inet4"}, {"table": 0},
                        {"cursor": {"offset": 0, "snapshot_digest": "sha256:" + "0" * 64}}):
            with self.assertRaises(Failure) as error:
                submit(invalid)
            self.assertEqual(error.exception.code, "InvalidArgument")

        original = submit({"limit": 1})
        page = read(original)
        cursor = page["next_cursor"]
        self.assertIsNotNone(cursor)
        boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        namespace = str(os.stat("/proc/self/ns/net").st_ino)
        user_namespace = str(os.stat("/proc/self/ns/user").st_ino)
        n.stop()
        n.start()
        self.assertEqual(read(original), page)
        digest = page["snapshot_digest"]
        rules = []
        while True:
            self.assertEqual(page["snapshot_digest"], digest)
            self.assertEqual((page["boot_id"], page["network_namespace_inode"], page["user_namespace_inode"]),
                             (boot, namespace, user_namespace))
            rules.extend(page["rules"])
            next_cursor = page["next_cursor"]
            if next_cursor is None:
                break
            self.assertEqual(next_cursor["offset"], len(rules))
            page = read(submit({"cursor": next_cursor, "limit": 4}))
        rejected({"cursor": cursor, "family": "inet"}, "ResyncRequired")
        rejected({"cursor": cursor}, "ResyncRequired", alternate)
        rejected({"cursor": dict(cursor, offset=len(rules))}, "InvalidArgument")

        reference = os.getenv("PSIOS_TEST_RULE_REFERENCE")
        if reference:
            self.assertFalse(Path("/usr/sbin/ip").exists())
            live = json.loads(Path(reference).read_bytes())
        else:
            live = [json.loads(subprocess.check_output(["/usr/sbin/ip", "-j", "-N", flag, "rule", "show"]))
                    for flag in ("-4", "-6")]
        for family, expected in zip(("inet", "inet6"), live):
            observed = [r for r in rules if r["family"] == family]
            self.assertEqual(read(submit({"family": family, "limit": 128}))["rules"], observed)
            self.assertEqual([(r["order"], r["priority"], r["table"], r["action"], r["goto_priority"], bool(r["flags"] & 2))
                              for r in observed],
                             [(i, r["priority"], int(r.get("table", 0)), int(r.get("action", 2 if "goto" in r else 1)),
                               r.get("goto"), "not" in r) for i, r in enumerate(expected)])

        if reference or os.getenv("PSIOS_TEST_MUTATE_RULES"):
            lookup, deny = sorted((r for r in rules if r["family"] == "inet" and r["priority"] == 100),
                                  key=lambda r: r["action"])
            address, rejected_ports = lookup["selectors"], deny["selectors"]
            self.assertEqual(address["source"], {"address": "192.0.2.0", "length": 24})
            self.assertEqual(address["destination"], {"address": "203.0.113.0", "length": 24})
            self.assertEqual((address["fwmark"], address["fwmask"], lookup["table"]), (0, 255, 1000))
            self.assertEqual((rejected_ports["input_interface"], rejected_ports["uid_range"], rejected_ports["ip_protocol"],
                              rejected_ports["source_port_range"], rejected_ports["destination_port_range"]),
                             ("lo", [1000, 1010], 6, [10000, 10020], [443, 443]))
            dual, = [r for r in rules if r["family"] == "inet6" and r["priority"] == 120]
            self.assertEqual(dual["selectors"]["source"], {"address": "2001:db8:1::", "length": 64})
            self.assertEqual(dual["selectors"]["destination"], {"address": "2001:db8:2::", "length": 64})
            by_priority = {r["priority"]: r for r in rules if r["family"] == "inet"}
            self.assertEqual(by_priority[250]["selectors"]["tunnel_id"], "9007199254740993")
            self.assertTrue(by_priority[270]["selectors"]["l3mdev"])
            self.assertEqual((by_priority[300]["suppress_prefixlen"], by_priority[300]["suppress_ifgroup"],
                              by_priority[300]["selectors"]["output_interface"]), (0, 5, "lo"))
        if os.getenv("PSIOS_TEST_MUTATE_RULES"):
            self.assertEqual([r["action"] for r in rules if r["family"] == "inet" and r["priority"] == 100], [1, 8])
            args = ["priority", "100", "from", "192.0.2.0/24", "to", "203.0.113.0/24", "fwmark", "0/255", "lookup", "1000"]
            subprocess.run(["/usr/sbin/ip", "-4", "rule", "del", *args], check=True)
            subprocess.run(["/usr/sbin/ip", "-4", "rule", "add", *args], check=True)
            rejected({"cursor": cursor}, "ResyncRequired")
            fresh = read(submit({"family": "inet"}))
            self.assertEqual([r["action"] for r in fresh["rules"] if r["priority"] == 100], [8, 1])
            self.assertEqual(read(original)["snapshot_digest"], digest)

    def test_route_lookup_matches_kernel_with_rule_selectors(self):
        n = Node("target/debug")
        self.addCleanup(n.close)
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        tool = "node.route_lookup"
        actions = ["task.submit", "task.read", tool]
        loopback = {"destination": "127.0.0.1"}
        route_reader, route_grant = self.actor(n, ["task.submit", "task.read", "node.routes", "node.routing_rules"], node)
        for token, grant in [(n.owner, "owner-delegation"), (route_reader, route_grant)]:
            with self.assertRaises(Failure) as denied:
                n.prepare(tool, loopback, token=token, delegation=grant)
            self.assertEqual(denied.exception.code, "PermissionDenied")
        wrong, scope = self.actor(n, actions, "profile-local")
        with self.assertRaises(Failure) as denied:
            n.call("Task.Submit", n.prepare(tool, loopback, token=wrong, delegation=scope), wrong)
        self.assertEqual(denied.exception.code, "PermissionDenied")
        token, grant = self.actor(n, actions, node)

        def look(params):
            request = n.prepare(tool, params, token=token, delegation=grant)
            task = n.wait(n.call("Task.Submit", request, token)["task_id"])
            self.assertEqual(task["lifecycle"], "Succeeded", task)
            ref, = task["result"]["artifact_refs"]
            return n.call("Artifact.Read", {"reference": ref}, token), ref

        for invalid in ({}, {"destination": "localhost"}, dict(loopback, source="::1"), dict(loopback, uid=2**32-1),
                        dict(loopback, output_ifindex=0), dict(loopback, source_port=5000)):
            with self.assertRaises(Failure) as bad:
                look(invalid)
            self.assertEqual(bad.exception.code, "InvalidArgument")
        local, ref = look(loopback)
        self.assertEqual(local["outcome"], "Resolved", local)
        self.assertEqual(local["lookup_uid"], os.getuid())
        self.assertEqual(local["network_namespace_inode"], str(os.stat("/proc/self/ns/net").st_ino))
        self.assertEqual(local["user_namespace_inode"], str(os.stat("/proc/self/ns/user").st_ino))
        lo = socket.if_nametoindex("lo")
        self.assertEqual((local["route"]["kind"], local["route"]["table"], local["route"]["output_ifindex"]),
                         (2, 255, lo))
        self.assertIsNone(local["kernel_errno"])
        bound, _ = look(dict(loopback, output_ifindex=lo))
        self.assertEqual((bound["outcome"], bound["route"]["output_ifindex"]), ("Resolved", lo))
        ipv6 = Path("/proc/sys/net/ipv6/conf/lo/disable_ipv6")
        if ipv6.exists() and ipv6.read_text().strip() == "0":
            local6, _ = look({"destination": "::1"})
            self.assertEqual(local6["outcome"], "Resolved", local6)
            self.assertEqual((local6["route"]["family"], local6["route"]["table"], local6["route"]["output_ifindex"]),
                             ("inet6", 255, lo))
        with self.assertRaises(Failure) as denied:
            n.call("Artifact.Read", {"reference": ref}, route_reader)
        self.assertEqual(denied.exception.code, "PermissionDenied")

        reference = os.getenv("PSIOS_TEST_LOOKUP_REFERENCE")
        mutate = os.getenv("PSIOS_TEST_MUTATE_ROUTES")
        if not (mutate or reference):
            return
        flows = [
            ({"destination": "198.20.0.7", "fwmark": 55}, ["-4", "route", "get", "198.20.0.7", "mark", "55"]),
            ({"destination": "2001:db8:3::5", "source": "2001:db8:1::2", "transport": "tcp", "source_port": 20000,
              "destination_port": 443}, ["-6", "route", "get", "2001:db8:3::5", "from", "2001:db8:1::2", "ipproto", "tcp",
                                        "sport", "20000", "dport", "443"]),
            ({"destination": "198.20.2.7", "uid": 4242}, ["-4", "route", "get", "198.20.2.7", "uid", "4242"]),
        ]
        if reference:
            self.assertFalse(Path("/usr/sbin/ip").exists())
            expected = json.loads(Path(reference).read_bytes())
        else:
            expected = [json.loads(subprocess.check_output(["/usr/sbin/ip", "-j", "-N", *args])) for _, args in flows]
        resolved = []
        for (params, _), (result,) in zip(flows, expected, strict=True):
            report, original = look(params)
            self.assertEqual(report["outcome"], "Resolved", report)
            self.assertEqual(report["lookup_uid"], params.get("uid", os.getuid()))
            self.assertIsNone(report["kernel_errno"])
            route = report["route"]
            self.assertEqual((route["destination"]["address"], route["table"], route["preferred_source"],
                              route["output_ifindex"], route["gateway"]),
                             (result["dst"], int(result["table"]), result["prefsrc"],
                              socket.if_nametoindex(result["dev"]), result["gateway"]))
            resolved.append((original, report))
        for params, code in (({"destination": "198.20.0.7"}, errno.ENETUNREACH),
                             ({"destination": "198.20.1.7"}, errno.EACCES),
                             ({"destination": "198.20.2.7"}, errno.ENETUNREACH)):
            rejected, _ = look(params)
            self.assertEqual((rejected["outcome"], rejected["route"], rejected["kernel_errno"]),
                             ("KernelRejected", None, code), rejected)
        other_port, _ = look(dict(flows[1][0], destination_port=444))
        self.assertEqual(other_port["outcome"], "KernelRejected", other_port)
        if mutate:
            args = ["/usr/sbin/ip", "-4", "route", "replace", "198.20.0.0/24", "dev", "psios-test", "table", "2000", "via"]
            subprocess.run(args + ["198.51.100.12"], check=True)
            try:
                updated, _ = look(flows[0][0])
                self.assertEqual(updated["route"]["gateway"], "198.51.100.12", updated)
                old, report = resolved[0]
                self.assertEqual(n.call("Artifact.Read", {"reference": old}, token), report)
            finally:
                subprocess.run(args + ["198.51.100.10"], check=True)

    def actor(self, n, actions, target):
        a = n.call("Agent.Register", {"name": "observer"})
        grant = n.call("Grant.Issue", {"principal_id": a["principal_id"], "actions": actions,
                       "tool_targets": [target], "ttl_seconds": 600, "units": 1000})
        return a["token"], grant["delegation_ref"]["resource_id"]
