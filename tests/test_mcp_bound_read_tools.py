"""Run both external MCP profiles against exact existing Volume and HostService tools."""
import base64
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import unittest

from support import Failure, key, target
from test_mcp_modern_stdio import PROFILE as MODERN, params, request
from test_mcp_stdio import McpFixture, PROFILE as LEGACY, Wire


class McpBoundReadToolsTests(McpFixture, unittest.TestCase):
    def issue(self, actions, targets=None):
        parameters = {"principal_id": self.agent["principal_id"], "actions": actions,
                      "units": 8, "ttl_seconds": 120}
        if targets is not None:
            parameters["tool_targets"] = targets
        return self.node.call("Grant.Issue", parameters)

    def bound_wire(self, grant, profile, *bindings):
        command = [*self.adapter_command(grant), "--protocol", profile, *bindings]
        client = Wire(command)
        self.addCleanup(client.close)
        if profile == LEGACY:
            client.initialize()
            return client.request

        def call(method, arguments=None):
            client.id += 1
            return request(client, client.id, method, params(arguments))

        return call

    def resolve(self, grant, *bindings):
        return self.node.call("Capability.ResolveLocalReadTools", {
            "delegation_id": grant["delegation_ref"]["resource_id"], "bindings": list(bindings),
        }, self.agent["token"])["tools"]

    def assert_input(self, task_id, expected):
        task = self.node.get(task_id)
        actual = self.node.call("Resource.Resolve", {"reference": task["resource"]["spec"]["input_ref"]})
        self.assertEqual(actual["value"], expected)

    def preparations(self):
        with closing(sqlite3.connect(self.node.state / "state.db")) as database:
            return database.execute("SELECT count(*) FROM task_preparations").fetchone()[0]

    def test_verified_node_diagnostics_require_explicit_node_scope_in_both_profiles(self):
        tools = ["node.inventory", "node.devices"]
        flags = ["--tool", tools[0], "--tool", tools[1]]
        bindings = [{"tool": tool} for tool in tools]
        actions = ["task.submit", "task.read", *tools]
        exact = self.issue(actions, [self.node_id])
        wide = self.issue(actions)
        foreign = self.issue(actions, [self.profile])
        n = self.node
        target_ref = n.get(self.node_id)["reference"]
        self.assertEqual(self.resolve(exact, *bindings), [
            {"tool": tool, "target_ref": target_ref} for tool in tools
        ])
        self.assertEqual(self.resolve(wide, *bindings), [])
        self.assertEqual(self.resolve(foreign, *bindings), [])
        for profile, arguments, denied in ((LEGACY, flags, wide),
                                            (MODERN, [*flags, "--volume-usage", "unavailable-volume"], foreign)):
            refused = self.bound_wire(denied, profile, *arguments)
            self.assertEqual(refused("tools/list")["result"]["tools"], [])
            for tool in tools:
                reply = refused("tools/call", {"name": tool})["result"]
                self.assertEqual(reply["structuredContent"], {"code": "PermissionDenied"}, reply)
        self.assertEqual(self.tasks(), [])
        self.assertEqual(self.preparations(), 0)

        for profile, arguments in ((LEGACY, flags), (MODERN, [*flags, "--volume-usage", "unavailable-volume"])):
            client = self.bound_wire(exact, profile, *arguments)
            listed = client("tools/list")["result"]["tools"]
            self.assertEqual([tool["name"] for tool in listed], tools)
            for tool in listed:
                self.assertTrue(tool["annotations"]["readOnlyHint"])
                schema = tool["inputSchema"]
                self.assertEqual((schema["type"], schema["additionalProperties"]), ("object", False))
                if tool["name"] == "node.devices":
                    self.assertEqual(set(schema["properties"]), {"limit", "cursor"})
                else:
                    self.assertNotIn("properties", schema)
            inventory = self.check_receipt(client("tools/call", {"name": tools[0]})["result"],
                                           tools[0], exact)["result"]
            self.assertEqual(inventory["boot_id"], n.call("Node.Get", {})["state"]["boot_id"])
            self.assertEqual(inventory["kernel_release"], os.uname().release)
            devices = self.check_receipt(client("tools/call", {"name": tools[1]})["result"],
                                         tools[1], exact)["result"]
            self.assertEqual(devices["boot_id"], inventory["boot_id"])
            self.assertEqual({row["name"] for row in devices["devices"]},
                             {device.name for device in Path("/sys/class/block").iterdir()})
            page = self.check_receipt(client("tools/call", {"name": tools[1], "arguments": {"limit": 1}})["result"],
                                      tools[1], exact)["result"]
            self.assertEqual(page["boot_id"], devices["boot_id"])
            self.assertEqual(page["devices"], devices["devices"][:1])
            self.assertTrue(page["snapshot_digest"].startswith("sha256:"))
        self.assertEqual(len(self.tasks()), 6)
        n.call("Grant.Revoke", target(exact["grant_ref"]))
        self.assertEqual(client("tools/list")["result"]["tools"], [])
        self.assertEqual(client("tools/call", {"name": tools[1]})["result"]["structuredContent"],
                         {"code": "PermissionDenied"})
        self.assertEqual(len(self.tasks()), 6)

    def test_list_only_bound_volume_pages_with_a_bounded_native_receipt(self):
        n = self.node
        tool = "volume.files"
        volume = self.create_owned_volume()
        volume_id = volume["resource_id"]
        root = n.state / "volumes" / volume_id / "data"
        directory = root / "reports"
        directory.mkdir()
        names = [f"{index:02d}-" + "\x01" * 240 for index in range(20)]
        for index, name in enumerate(names):
            (directory / name).write_bytes(b"x" * index)
        other = n.perform("volume.create", {"name": "not-in-the-mcp-binding"})
        actions = ["task.submit", "task.read", tool]
        binding = {"tool": tool, "target_id": volume_id}
        self.assertEqual(self.resolve(self.issue(actions), binding), [])
        self.assertEqual(self.resolve(self.issue(actions, [other["resource_id"]]), binding), [])
        exact = self.issue(actions, [volume_id])
        self.assertEqual(self.resolve(exact, binding), [{"tool": tool, "target_ref": volume}])

        for profile in (LEGACY, MODERN):
            with self.subTest(profile=profile):
                client = self.bound_wire(exact, profile, "--volume-files", volume_id)
                listed, = client("tools/list")["result"]["tools"]
                self.assertEqual(listed["name"], tool)
                self.assertEqual(set(listed["inputSchema"]["properties"]),
                                 {"path", "limit", "after", "volume_version"})
                for arguments in ({"target": target(other)}, {"path": "../reports"},
                                  {"limit": 33}, {"after": names[0]}):
                    denied = client("tools/call", {"name": tool, "arguments": arguments})["result"]
                    self.assertEqual(denied["structuredContent"], {"code": "InvalidArgument"}, denied)

                query = {"path": "reports"}
                entries = []
                continuation = None
                for index in range(4):
                    response = client("tools/call", {"name": tool, "arguments": query})["result"]
                    receipt = self.check_receipt(response, tool, exact)
                    self.assert_input(receipt["task_id"], {"target": target(volume), "path": "reports",
                        "after": query.get("after"), "limit": 32, "max_result_bytes": 12 * 1024})
                    page = receipt["result"]
                    self.assertLessEqual(len(json.dumps(page, ensure_ascii=False,
                        separators=(",", ":")).encode("utf-8")), 12 * 1024)
                    self.assertEqual(page["volume_version"], volume["version"])
                    entries.extend(page["entries"])
                    if page["next_after"] is None:
                        break
                    self.assertEqual(page["next_after"], page["entries"][-1]["name"])
                    query = {"path": "reports", "after": page["next_after"], "volume_version": page["volume_version"]}
                    if index == 0:
                        continuation = query
                self.assertIsNotNone(continuation)
                self.assertEqual(entries, [{"name": name, "kind": "file", "bytes": str(index)}
                                           for index, name in enumerate(names)])

        fresh = n.perform("volume.write", {**target(volume), "name": "advance", "content": "revision"})["volume_ref"]
        self.assertNotEqual(fresh["version"], continuation["volume_version"])
        denied = client("tools/call", {"name": tool, "arguments": continuation})["result"]
        self.assertEqual(denied["structuredContent"], {"code": "Conflict"}, denied)
        n.call("Grant.Revoke", target(exact["grant_ref"]))
        self.assertEqual(client("tools/list")["result"]["tools"], [])
        denied = client("tools/call", {"name": tool, "arguments": {"path": "reports"}})["result"]
        self.assertEqual(denied["structuredContent"], {"code": "PermissionDenied"})

    def test_bound_binary_chunks_reject_a_changed_file_with_the_same_volume_revision(self):
        n = self.node
        tool = "volume.read_chunk"
        volume = self.create_owned_volume()
        volume_id = volume["resource_id"]
        root = n.state / "volumes" / volume_id / "data"
        directory = root.joinpath(*(['"' * 240] * 8))
        directory.mkdir(parents=True)
        file = directory / "binary"
        path = str(file.relative_to(root))
        content = bytes(range(256)) * 25
        file.write_bytes(content)
        maximum = 6 * 1024
        binding = {"tool": tool, "target_id": volume_id}
        actions = ["task.submit", "task.read", tool]
        self.assertEqual(self.resolve(self.issue(actions), binding), [])
        exact = self.issue(actions, [volume_id])
        self.assertEqual(self.resolve(exact, binding), [{"tool": tool, "target_ref": volume}])
        with self.assertRaises(Failure) as missing:
            n.call("McpTask.Start", {"endpoint": "binary-reader",
                "delegation_id": exact["delegation_ref"]["resource_id"], "invocation_key": key(),
                "binding": binding, "ttl_seconds": 90}, self.agent["token"])
        self.assertEqual(missing.exception.code, "InvalidArgument")

        continuation = None
        for profile in (LEGACY, MODERN):
            with self.subTest(profile=profile):
                client = self.bound_wire(exact, profile, "--volume-read-chunk", volume_id)
                listed, = client("tools/list")["result"]["tools"]
                self.assertEqual(listed["inputSchema"]["required"], ["path"])
                self.assertEqual(listed["inputSchema"]["properties"]["limit"]["maximum"], maximum)
                for invalid in ({}, {"path": path, "offset": 1}, {"path": path, "limit": maximum + 1},
                                {"path": "../outside"}, {"path": path, "target": target(volume)}):
                    rejected = client("tools/call", {"name": tool, "arguments": invalid})["result"]
                    self.assertEqual(rejected["structuredContent"], {"code": "InvalidArgument"}, rejected)
                response = client("tools/call", {"name": tool, "arguments": {"path": path}})["result"]
                first = self.check_receipt(response, tool, exact)["result"]
                self.assertLess(len(response["content"][0]["text"].encode("utf-8")), 16 * 1024)
                self.assertEqual((first["volume_version"], first["offset"], first["next_offset"], first["eof"]),
                                 (volume["version"], 0, maximum, False))
                self.assertEqual(base64.b64decode(first["data_base64"], validate=True), content[:maximum])
                self.assertEqual(first["digest"], "sha256:" + hashlib.sha256(content[:maximum]).hexdigest())
                continuation = {"path": path, "offset": first["next_offset"],
                                "file_revision": first["file_revision"], "volume_version": first["volume_version"]}
                response = client("tools/call", {"name": tool, "arguments": continuation})["result"]
                last = self.check_receipt(response, tool, exact)["result"]
                self.assertEqual((last["next_offset"], last["total_bytes"], last["eof"]),
                                 (len(content), len(content), True))
                self.assertEqual(last["file_revision"], first["file_revision"])
                self.assertEqual(base64.b64decode(last["data_base64"], validate=True), content[maximum:])

        substitute = directory / "replacement"
        substitute.write_bytes(content)
        substitute.replace(file)
        self.assertEqual(n.get(volume_id)["reference"], volume)
        denied = client("tools/call", {"name": tool, "arguments": continuation})["result"]
        self.assertTrue(denied["isError"], denied)
        self.assertEqual(denied["structuredContent"]["code"], "Conflict", denied)
        failed = n.call("Task.Get", {"id": denied["structuredContent"]["task_id"]})
        self.assertEqual((failed["lifecycle"], failed["result"]["artifact_refs"]), ("Failed", []))
        fresh = client("tools/call", {"name": tool, "arguments": {"path": path, "limit": 1}})["result"]
        first = self.check_receipt(fresh, tool, exact)["result"]
        self.assertNotEqual(first["file_revision"], continuation["file_revision"])

    def test_measure_only_the_fixed_owned_volume_and_resolve_its_current_revision(self):
        n = self.node
        volume = self.create_owned_volume()
        volume = n.perform("volume.write", {**target(volume), "name": "metered", "content": "four"})["volume_ref"]
        binding = {"tool": "volume.usage", "target_id": volume["resource_id"]}
        foreign = n.perform("volume.create", {"name": "mcp-owner-only"})
        foreign_binding = {"tool": "volume.usage", "target_id": foreign["resource_id"]}
        actions = ["task.submit", "task.read", "volume.usage", "system.status"]
        grant = self.issue(actions, [volume["resource_id"], self.profile])
        wide = self.issue(actions)
        scoped_to_foreign = self.issue(actions, [foreign["resource_id"]])
        scoped_to_wrong_kind = self.issue(actions, [self.node_id])
        self.issue(["task.read"])
        baseline = self.tasks()
        preparations = self.preparations()
        self.assertEqual(self.resolve(grant, binding), [{"tool": "volume.usage", "target_ref": volume}])
        self.assertEqual(self.resolve(grant, foreign_binding), [])
        self.assertEqual(self.resolve(wide, binding), [])
        self.assertEqual(self.resolve(scoped_to_foreign, foreign_binding), [])
        self.assertEqual(self.resolve(scoped_to_wrong_kind, {"tool": "volume.usage", "target_id": self.node_id}), [])
        with self.assertRaises(Failure) as duplicate:
            self.resolve(grant, binding, foreign_binding)
        self.assertEqual(duplicate.exception.code, "InvalidArgument")

        results = []
        clients = []
        for profile in (LEGACY, MODERN):
            cli = self.bound_wire(grant, profile, "--volume-usage", volume["resource_id"])
            clients.append(cli)
            [listed] = cli("tools/list")["result"]["tools"]
            self.assertEqual(listed["name"], "volume.usage")
            self.assertEqual(listed["inputSchema"], {"type": "object", "additionalProperties": False})
            self.assertEqual(cli("tools/call", {"name": "system.status"})["error"]["code"], -32602)
            for arguments in ({"path": "nested"}, {"target": target(foreign)}):
                refused = cli("tools/call", {"name": "volume.usage", "arguments": arguments})["result"]
                self.assertTrue(refused["isError"])
                self.assertEqual(refused["structuredContent"], {"code": "InvalidArgument"})
        self.assertEqual(self.tasks(), baseline)
        self.assertEqual(self.preparations(), preparations)
        for cli in clients:
            observed = cli("tools/call", {"name": "volume.usage"})["result"]
            report = self.check_receipt(observed, "volume.usage", grant)
            results.append(report)
            self.assert_input(report["task_id"], {"target": target(volume)})
            value = report["result"]
            self.assertEqual((value["volume_ref"], value["path"], value["files"], value["logical_file_bytes"]),
                             (volume, "", 1, "4"))

        mixed = self.bound_wire(grant, MODERN, "--tool", "system.status", "--volume-usage", volume["resource_id"])
        self.assertEqual([item["name"] for item in mixed("tools/list")["result"]["tools"]], ["system.status", "volume.usage"])
        delegated = grant["delegation_ref"]["resource_id"]
        token = self.agent["token"]
        self.assertEqual(n.call("Capability.LocalReadTools", {"delegation_id": delegated,
                         "tools": ["system.status"]}, token), {"tools": ["system.status"]})
        stale = n.prepare("volume.usage", {"target": target(volume)}, token=token, delegation=delegated)
        fresh = n.perform("volume.write", {**target(volume), "name": "metered", "content": "longer"})["volume_ref"]
        self.assertNotEqual(fresh, volume)
        previous_tasks = self.tasks()
        with self.assertRaises(Failure) as obsolete:
            n.call("Task.Submit", stale, token)
        self.assertEqual(obsolete.exception.code, "Conflict")
        self.assertEqual(self.tasks(), previous_tasks)
        self.assertEqual(self.resolve(grant, binding), [{"tool": "volume.usage", "target_ref": fresh}])
        report = self.check_receipt(mixed("tools/call", {"name": "volume.usage"})["result"], "volume.usage", grant)
        self.assert_input(report["task_id"], {"target": target(fresh)})
        self.assertEqual(report["result"]["logical_file_bytes"], "6")

        n.call("Grant.Revoke", target(grant["grant_ref"]))
        for cli in clients:
            self.assertEqual(cli("tools/list")["result"]["tools"], [])
            refused = cli("tools/call", {"name": "volume.usage"})["result"]
            self.assertEqual(refused["structuredContent"], {"code": "PermissionDenied"})
        previous = results[0]
        settled = n.call("Task.Get", {"id": previous["task_id"]}, token)
        [artifact] = settled["result"]["artifact_refs"]
        self.assertEqual(n.call("Artifact.Read", {"reference": artifact}, token), previous["result"])

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_MCP_SYSTEMD") == "1", "requires a real user systemd manager")
    def test_inspect_only_the_registered_systemd_unit_in_both_profiles(self):
        n = self.node
        units = Path(f"/run/user/{os.geteuid()}/systemd/user")
        units.mkdir(mode=0o700, parents=True, exist_ok=True)
        primary = f"psios-mcp-bound-{key()}.service"
        unrelated = f"psios-mcp-bound-{key()}.service"
        systemctl = ["systemctl", "--user"]
        files = [units / unit for unit in (primary, unrelated)]

        def cleanup():
            for path in files:
                path.unlink(missing_ok=True)
            subprocess.run([*systemctl, "daemon-reload"], check=True, timeout=15)

        self.addCleanup(cleanup)
        for path in files:
            path.write_text("[Service]\nType=oneshot\nExecStart=/bin/true\n")
        subprocess.run([*systemctl, "daemon-reload"], check=True, timeout=15)

        configuration = json.loads(n.config.read_text())
        configuration["services"] = [{"unit": unit, "manager": "user", "allow_control": False}
                                     for unit in (primary, unrelated)]
        n.stop()
        n.config.write_text(json.dumps(configuration))
        n.start()
        service = n.get(f"service-{primary}")["reference"]
        other = n.get(f"service-{unrelated}")["reference"]
        binding = {"tool": "service.inspect", "target_id": service["resource_id"]}
        grant = self.issue(["task.submit", "task.read", "service.inspect"], [service["resource_id"]])
        self.assertEqual(self.resolve(grant, binding), [{"tool": "service.inspect", "target_ref": service}])
        self.assertEqual(self.resolve(grant, {"tool": "service.inspect", "target_id": other["resource_id"]}), [])
        baseline = self.tasks()
        for profile in (LEGACY, MODERN):
            denied = self.bound_wire(grant, profile, "--service-inspect", other["resource_id"])
            self.assertEqual(denied("tools/list")["result"]["tools"], [])
            self.assertEqual(denied("tools/call", {"name": "service.inspect"})["result"]["structuredContent"],
                             {"code": "PermissionDenied"})
        self.assertEqual(self.tasks(), baseline)
        for profile in (LEGACY, MODERN):
            cli = self.bound_wire(grant, profile, "--service-inspect", service["resource_id"])
            self.assertEqual([item["name"] for item in cli("tools/list")["result"]["tools"]], ["service.inspect"])
            refused = cli("tools/call", {"name": "service.inspect", "arguments": {"unit": unrelated}})["result"]
            self.assertEqual(refused["structuredContent"], {"code": "InvalidArgument"})
            observed = cli("tools/call", {"name": "service.inspect"})["result"]
            report = self.check_receipt(observed, "service.inspect", grant)
            self.assert_input(report["task_id"], target(service))
            self.assertEqual((report["result"]["unit"], report["result"]["manager"], report["result"]["load_state"]),
                             (primary, "user", "loaded"))
        self.assertEqual(len(self.tasks()), len(baseline) + 2)
        n.stop()
        configuration["services"].pop(0)
        n.config.write_text(json.dumps(configuration))
        n.start()
        self.assertEqual(self.resolve(grant, binding), [])
