"""Run both external MCP profiles against exact existing Volume and HostService tools."""
from contextlib import closing
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

    def create_owned_volume(self):
        n = self.node
        token = self.agent["token"]
        create = self.issue(["task.submit", "task.read", "volume.create"])
        prepared = n.prepare("volume.create", {"name": "mcp-agent-owned"}, token=token,
                             delegation=create["delegation_ref"]["resource_id"])
        made = n.wait(n.call("Task.Submit", prepared, token)["task_id"], autoapprove=True)
        self.assertEqual(made["lifecycle"], "Succeeded", made)
        [artifact] = made["result"]["artifact_refs"]
        return n.call("Artifact.Read", {"reference": artifact}, token)

    def assert_input(self, task_id, expected):
        task = self.node.get(task_id)
        actual = self.node.call("Resource.Resolve", {"reference": task["resource"]["spec"]["input_ref"]})
        self.assertEqual(actual["value"], expected)

    def preparations(self):
        with closing(sqlite3.connect(self.node.state / "state.db")) as database:
            return database.execute("SELECT count(*) FROM task_preparations").fetchone()[0]

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
