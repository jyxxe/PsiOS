"""Exercise the 2026-07-28 Tasks extension across real STDIO and Core restarts."""
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from network_resolver import RESOLVER
from test_mcp_modern_stdio import CAPABILITIES_KEY, PROFILE, params, request, send
from test_mcp_stdio import McpFixture, PROFILE as LEGACY, Wire
from test_pci_observation import DEVICES as PCI_DEVICES, linux_devices


EXTENSION = "io.modelcontextprotocol/tasks"
INVOCATION = "io.psios/mcpInvocationKey"


def task_params(fields=None, *, invocation=None, capability=True):
    body = params(fields)
    if capability:
        body["_meta"][CAPABILITIES_KEY]["extensions"] = {EXTENSION: {}}
    if invocation is not None:
        body["_meta"][INVOCATION] = invocation
    return body


class McpTaskFixture(McpFixture):
    def task_wire(self, grant, endpoint="diagnostics", ttl=300, token_file=None, extra=()):
        command = [*self.adapter_command(grant), "--protocol", PROFILE, *extra]
        if token_file is not None:
            command[command.index("--token-file") + 1] = str(token_file)
        if endpoint is not None:
            command.extend(["--tasks-endpoint", endpoint, "--tasks-ttl-seconds", str(ttl)])
        wire = Wire(command)
        self.addCleanup(wire.close)
        return wire

    def task_call(self, wire, method, fields=None, *, invocation=None, capability=True):
        wire.id += 1
        return request(wire, wire.id, method, task_params(fields, invocation=invocation, capability=capability))

    def begin(self, wire, key):
        return self.task_call(wire, "tools/call", {"name": "system.status"}, invocation=key)["result"]

    def wait_status(self, wire, handle, status="completed"):
        cutoff = time.monotonic() + 10
        while True:
            view = self.task_call(wire, "tasks/get", {"taskId": handle})["result"]
            self.assertEqual(view["resultType"], "complete")
            if view["status"] == status:
                return view
            self.assertEqual(view["status"], "working", view)
            self.assertLess(time.monotonic(), cutoff, view)
            time.sleep(0.25)

    def scheduling(self, mode):
        ref = self.node.call("Node.Get", {})["reference"]
        self.node.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})


class McpTaskTests(McpTaskFixture, unittest.TestCase):
    @unittest.skipUnless(PCI_DEVICES.is_dir(), "requires Linux PCI sysfs")
    def test_pci_observation_pages_require_the_exact_node_scope_and_resume(self):
        expected = linux_devices()
        if len(expected) < 2:
            self.skipTest("requires two PCI devices for continuation")
        n = self.node
        tool = "node.pci"
        flags = ("--tool", tool)
        grant = self.grant([self.node_id], units=8, tools=[tool])
        wrong = self.grant([self.profile], tools=[tool])

        def legacy(access, extra=flags):
            wire = Wire([*self.adapter_command(access), "--protocol", LEGACY, *extra])
            self.addCleanup(wire.close)
            wire.initialize()
            return wire

        def direct(wire, arguments=None):
            fields = {"name": tool}
            if arguments is not None:
                fields["arguments"] = arguments
            return wire.request("tools/call", fields)["result"]

        self.assertEqual(legacy(wrong).request("tools/list")["result"]["tools"], [])
        self.assertEqual(legacy(grant, ()).request("tools/list")["result"]["tools"], [])
        peer = legacy(grant)
        listed, = peer.request("tools/list")["result"]["tools"]
        self.assertTrue(listed["annotations"]["readOnlyHint"])
        properties = listed["inputSchema"]["properties"]
        self.assertEqual((properties["limit"]["maximum"], properties["cursor"]["properties"]["offset"]["maximum"]),
                         (32, 255))
        self.assertEqual(direct(peer, {"path": "/sys"})["structuredContent"], {"code": "InvalidArgument"})
        first = self.check_receipt(direct(peer, {"limit": 1}), tool, grant)["result"]
        self.assertEqual(first["devices"], expected[:1])
        cursor = first["next_cursor"]
        second = self.check_receipt(direct(peer, {"limit": 1, "cursor": cursor}), tool, grant)["result"]
        self.assertEqual(second["devices"], expected[1:2])
        other = legacy(self.grant([self.node_id], tools=[tool]))
        self.assertEqual(direct(other, {"limit": 1, "cursor": cursor})["structuredContent"]["code"], "ResyncRequired")

        invocation = key()
        arguments = {"name": tool, "arguments": {"limit": 1, "cursor": cursor}}
        modern = self.task_wire(grant, extra=flags)
        task = self.task_call(modern, "tools/call", arguments, invocation=invocation)["result"]["taskId"]
        settled = self.wait_status(modern, task)
        page = self.check_receipt(settled["result"], tool, grant)["result"]
        self.assertEqual((page["devices"], page["snapshot_digest"]), (expected[1:2], first["snapshot_digest"]))
        modern.close()
        n.stop(signal.SIGKILL)
        n.start()
        resumed = self.task_wire(grant, extra=flags)
        self.assertEqual(self.task_call(resumed, "tools/call", arguments, invocation=invocation)["result"]["taskId"], task)
        self.assertEqual(self.wait_status(resumed, task), settled)
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assertEqual(self.task_call(resumed, "tools/list")["result"]["tools"], [])
        self.assertEqual(self.task_call(resumed, "tasks/get", {"taskId": task})["error"]["code"], -32602)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_RESOLVER_FIXTURE"), "requires a resolver file in a private mount namespace")
    def test_resolver_diagnostic_requires_scoped_mcp_and_recovers_modern_task(self):
        fixture = Path(os.environ["PSIOS_TEST_RESOLVER_FIXTURE"])
        assert fixture.resolve() != RESOLVER.resolve() and fixture.samefile(RESOLVER), fixture
        fixture.write_text("search hidden.invalid\nnameserver 192.0.2.53\nnameserver fe80::53%eth0\n")
        n = self.node
        tool = "node.resolver"
        exact = self.grant([self.node_id], units=4, tools=[tool])
        foreign = self.grant([self.profile], tools=[tool])
        flags = ("--tool", tool)

        def legacy(grant):
            wire = Wire([*self.adapter_command(grant), "--protocol", LEGACY, *flags])
            self.addCleanup(wire.close)
            wire.initialize()
            return wire

        def sample(wire, invocation):
            return self.task_call(wire, "tools/call", {"name": tool}, invocation=invocation)["result"]

        blocked = legacy(foreign)
        self.assertEqual(blocked.request("tools/list")["result"]["tools"], [])
        refused = blocked.request("tools/call", {"name": tool})["result"]
        self.assertEqual(refused["structuredContent"], {"code": "PermissionDenied"})
        direct = legacy(exact)
        [listed] = direct.request("tools/list")["result"]["tools"]
        self.assertEqual(listed["name"], tool)
        self.assertEqual(listed["inputSchema"], {"type": "object", "additionalProperties": False})
        self.assertTrue(listed["annotations"]["readOnlyHint"])
        rejected = direct.request("tools/call", {"name": tool, "arguments": {"path": "/etc/passwd"}})["result"]
        self.assertEqual(rejected["structuredContent"], {"code": "InvalidArgument"})
        self.assertEqual(self.tasks(), [])
        reply = direct.request("tools/call", {"name": tool})["result"]
        observation = self.check_receipt(reply, tool, exact)["result"]
        self.assertEqual(observation["nameservers"], [{"address": "192.0.2.53"},
                                                     {"address": "fe80::53", "zone": "eth0"}])
        self.assertNotIn("hidden.invalid", json.dumps(observation))
        default = Wire(self.adapter_command(exact))
        self.addCleanup(default.close)
        default.initialize()
        self.assertEqual(default.request("tools/list")["result"]["tools"], [])

        modern = self.task_wire(exact, extra=flags)
        [listed] = self.task_call(modern, "tools/list")["result"]["tools"]
        self.assertEqual(listed["name"], tool)
        invocation = key()
        first = sample(modern, invocation)["taskId"]
        settled = self.wait_status(modern, first)
        prior = self.check_receipt(settled["result"], tool, exact)["result"]
        self.assertEqual(prior["nameservers"], observation["nameservers"])
        self.assertEqual(len(self.tasks()), 2)
        modern.close()
        n.stop(signal.SIGKILL)
        fixture.write_text("nameserver 2001:db8::9\n")
        n.start()
        resumed = self.task_wire(exact, extra=flags)
        self.assertEqual(sample(resumed, invocation)["taskId"], first)
        self.assertEqual(self.wait_status(resumed, first), settled)
        self.assertEqual(len(self.tasks()), 2)
        current = sample(resumed, key())["taskId"]
        received = self.check_receipt(self.wait_status(resumed, current)["result"], tool, exact)["result"]
        self.assertEqual(received["nameservers"], [{"address": "2001:db8::9"}])
        n.call("Grant.Revoke", target(exact["grant_ref"]))
        self.assertEqual(self.task_call(resumed, "tools/list")["result"]["tools"], [])
        self.assertEqual(self.task_call(resumed, "tasks/get", {"taskId": first})["error"]["code"], -32602)
        self.assertEqual(len(self.tasks()), 3)

    def test_device_page_input_and_cursor_survive_adapter_and_daemon_restart(self):
        n = self.node
        tool = "node.devices"
        grant = self.grant([self.node_id], units=4, tools=[tool])
        client = self.task_wire(grant, extra=("--tool", tool))
        listed, = self.task_call(client, "tools/list")["result"]["tools"]
        self.assertEqual(listed["name"], tool)
        self.assertEqual(listed["inputSchema"]["properties"]["limit"]["maximum"], 32)
        invocation = key()

        def start(wire, query, attempt):
            return self.task_call(wire, "tools/call", {"name": tool, "arguments": query}, invocation=attempt)["result"]

        first = start(client, {"limit": 1}, invocation)["taskId"]
        settled = self.wait_status(client, first)
        first_result = self.check_receipt(settled["result"], tool, grant)["result"]
        self.assertLessEqual(len(first_result["devices"]), 1)
        cursor = first_result["next_cursor"]
        client.close()
        n.stop(signal.SIGKILL)
        offline = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(offline["database_format"], 139)
        self.assertEqual(offline["sqlite_integrity"], "Verified")
        n.start()

        resumed = self.task_wire(grant, extra=("--tool", tool))
        self.assertEqual(start(resumed, {"limit": 1}, invocation)["taskId"], first)
        self.assertEqual(start(resumed, {"limit": 2}, invocation)["structuredContent"], {"code": "IdempotencyConflict"})
        self.assertEqual(self.wait_status(resumed, first), settled)
        self.assertEqual(len(self.tasks()), 1)
        if cursor is not None:
            following = start(resumed, {"limit": 1, "cursor": cursor}, key())["taskId"]
            page = self.check_receipt(self.wait_status(resumed, following)["result"], tool, grant)["result"]
            self.assertEqual(page["snapshot_digest"], first_result["snapshot_digest"])
            self.assertEqual(len(page["devices"]), 1)
            self.assertGreater(page["devices"][0]["name"], first_result["devices"][0]["name"])
            self.assertEqual(len(self.tasks()), 2)
        without_tool = self.task_wire(grant, extra=("--tool", "node.inventory"))
        self.assertEqual(self.task_call(without_tool, "tasks/get", {"taskId": first})["error"]["code"], -32602)
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assertEqual(self.task_call(resumed, "tasks/get", {"taskId": first})["error"]["code"], -32602)

    @unittest.skipUnless(os.getenv("PSIOS_MCP_PAGE_PREVIOUS_BIN_DIR"), "requires pinned format-138 binaries")
    def test_format_138_unpaged_handle_is_preserved_and_old_writer_is_fenced(self):
        node = Node(os.environ["PSIOS_MCP_PAGE_PREVIOUS_BIN_DIR"])
        self.addCleanup(node.close)
        actor = node.call("Agent.Register", {"name": "device-page-upgrade"})
        token = actor["token"]
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        grant = node.call("Grant.Issue", {"principal_id": actor["principal_id"], "tool_targets": [node_id],
            "actions": ["task.submit", "task.read", "node.devices"], "units": 2, "ttl_seconds": 120})
        scope = {"endpoint": "device-inventory", "delegation_id": grant["delegation_ref"]["resource_id"]}
        binding = {"tool": "node.devices"}
        start = {**scope, "binding": binding, "invocation_key": key(), "ttl_seconds": 120}
        handle = node.call("McpTask.Start", start, token)["taskId"]
        [reference] = node.call("Resource.List", {"kind": "Task"})["items"]
        self.assertEqual(node.wait(reference["resource_id"])["lifecycle"], "Succeeded")
        reference = node.get(reference["resource_id"])["reference"]
        access = {**scope, "handle": handle, "bindings": [binding]}
        original = node.call("McpTask.Get", access, token)
        self.assertEqual(original["tool_result"]["task_id"], reference["resource_id"])
        self.assertNotIn("next_cursor", original["tool_result"]["result"])

        node.stop(signal.SIGKILL)
        old = node.daemon.copy()
        node.binaries = self.node.binaries
        node.daemon[0] = str(node.binaries / "psiosd")
        pending = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertEqual((pending["node_id"], pending["database_format"], pending["migration_required"]),
                         (node_id, 138, True))
        node.start()
        self.assertEqual(node.call("McpTask.Start", start, token)["taskId"], handle)
        self.assertEqual(node.call("McpTask.Get", access, token), original)
        self.assertEqual(node.call("Resource.List", {"kind": "Task"})["items"], [reference])
        node.stop()
        before = (node.witness / "head").read_bytes()
        blocked = subprocess.run([*old, "serve"], capture_output=True, timeout=10)
        self.assertNotEqual(blocked.returncode, 0, blocked)
        self.assertIn(b"SchemaVersionUnsupported", blocked.stderr)
        self.assertEqual((node.witness / "head").read_bytes(), before)

    @unittest.skipUnless(os.getenv("PSIOS_MCP_NODE_PREVIOUS_BIN_DIR"), "requires pinned format-137 binaries")
    def test_format_137_keeps_original_device_receipt_while_enabling_mcp_discovery(self):
        node = Node(os.environ["PSIOS_MCP_NODE_PREVIOUS_BIN_DIR"])
        self.addCleanup(node.close)
        actor = node.call("Agent.Register", {"name": "retained-inventory-caller"})
        scope = node.call("Node.Get", {})["reference"]["resource_id"]
        grant = node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": ["task.submit", "task.read", "node.devices"],
            "tool_targets": [scope], "units": 3, "ttl_seconds": 90,
        })
        discovery = {"delegation_id": grant["delegation_ref"]["resource_id"], "tools": ["node.devices"]}
        with self.assertRaises(Failure) as unsupported:
            node.call("Capability.LocalReadTools", discovery, actor["token"])
        self.assertEqual(unsupported.exception.code, "InvalidArgument")
        prepared = node.prepare("node.devices", {}, delegation=discovery["delegation_id"], token=actor["token"])
        task = node.call("Task.Submit", prepared, actor["token"])["task_id"]
        settled = node.wait(task)
        self.assertEqual(settled["lifecycle"], "Succeeded", settled)
        artifact, = settled["result"]["artifact_refs"]
        original = node.call("Artifact.Read", {"reference": artifact}, actor["token"])
        node.stop(signal.SIGKILL)
        old = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertEqual((old["database_format"], old["migration_required"]), (137, False))
        node.binaries = self.node.binaries
        node.daemon[0] = str(node.binaries / "psiosd")
        pending = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertEqual((pending["node_id"], pending["database_format"], pending["migration_required"]),
                         (old["node_id"], 137, True))
        node.start()
        self.assertEqual(node.call("Capability.LocalReadTools", discovery, actor["token"]), {"tools": ["node.devices"]})
        self.assertEqual(node.call("Task.Get", {"id": task}, actor["token"]), settled)
        self.assertEqual(node.call("Artifact.Read", {"reference": artifact}, actor["token"]), original)

    def test_verified_device_inventory_handle_survives_restart_and_rejects_older_writer(self):
        n = self.node
        tool = "node.devices"
        grant = n.call("Grant.Issue", {
            "principal_id": self.agent["principal_id"], "actions": ["task.submit", "task.read", tool],
            "tool_targets": [self.node_id], "units": 3, "ttl_seconds": 90,
        })
        invocation = key()
        self.scheduling("Cordoned")
        first = self.task_wire(grant, extra=("--tool", tool))
        accepted = self.task_call(first, "tools/call", {"name": tool}, invocation=invocation)["result"]
        self.assertEqual((accepted["resultType"], accepted["status"]), ("task", "working"), accepted)
        handle = accepted["taskId"]
        original, = self.tasks()
        first.close()
        n.stop(signal.SIGKILL)
        checked = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(checked["database_format"], 138)
        self.assertEqual(checked["sqlite_integrity"], "Verified")
        if previous := os.getenv("PSIOS_MCP_NODE_PREVIOUS_BIN_DIR"):
            before = (n.witness / "head").read_bytes()
            rejected = subprocess.run([Path(previous) / "psiosd", *n.daemon[1:], "serve"],
                                      capture_output=True, text=True, timeout=10)
            self.assertNotEqual(rejected.returncode, 0, rejected)
            self.assertIn("SchemaVersionUnsupported", rejected.stderr)
            self.assertEqual((n.witness / "head").read_bytes(), before)
        n.start()
        recovered = self.task_wire(grant, extra=("--tool", tool))
        repeated = self.task_call(recovered, "tools/call", {"name": tool}, invocation=invocation)["result"]
        self.assertEqual(repeated["taskId"], handle)
        self.assertEqual(self.tasks(), [original])
        self.scheduling("Enabled")
        stale = self.wait_status(recovered, handle)
        self.assertEqual(stale["result"]["structuredContent"], {
            "code": "ApprovalInvalidated", "task_id": original["resource_id"],
        })
        self.assertEqual(n.get(original["resource_id"])["value"]["result"]["artifact_refs"], [])
        fresh_key = key()
        fresh = self.task_call(recovered, "tools/call", {"name": tool}, invocation=fresh_key)["result"]["taskId"]
        self.assertNotEqual(fresh, handle)
        settled = self.wait_status(recovered, fresh)
        report = self.check_receipt(settled["result"], tool, grant)
        self.assertEqual(report["result"]["boot_id"], n.call("Node.Get", {})["state"]["boot_id"])
        self.assertEqual({task["resource_id"] for task in self.tasks()}, {original["resource_id"], report["task_id"]})
        recovered.close()
        n.stop(signal.SIGKILL)
        n.start()
        reconnected = self.task_wire(grant, extra=("--tool", tool))
        self.assertEqual(self.wait_status(reconnected, fresh), settled)
        self.assertEqual(self.task_call(reconnected, "tools/call", {"name": tool},
                                        invocation=fresh_key)["result"]["taskId"], fresh)
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assertEqual(self.task_call(reconnected, "tasks/get", {"taskId": fresh})["error"]["code"], -32602)

    def test_lost_stdio_response_rejoins_one_task_across_daemon_and_adapter_restart(self):
        n = self.node
        grant = self.grant([self.profile])
        invocation = key()
        first = self.task_wire(grant)
        discovered = self.task_call(first, "server/discover", capability=False)["result"]
        self.assertEqual(discovered["capabilities"]["extensions"], {EXTENSION: {}})
        blocked = self.task_call(first, "tools/call", {"name": "system.status"},
                                 invocation=invocation, capability=False)
        self.assertEqual(blocked["error"]["code"], -32021)
        self.assertEqual(blocked["error"]["data"], {"requiredCapabilities": {"extensions": {EXTENSION: {}}}})
        self.assertEqual(self.tasks(), [])

        self.scheduling("Cordoned")
        send(first, "response-not-consumed", "tools/call", task_params({"name": "system.status"}, invocation=invocation))
        cutoff = time.monotonic() + 5
        while not self.tasks():
            self.assertLess(time.monotonic(), cutoff, "no durable Task accepted")
            time.sleep(0.02)
        [original] = self.tasks()
        task_id = original["resource_id"]
        first.close()
        n.stop(signal.SIGKILL)
        checked = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(checked["database_format"], 122)
        n.start()
        second = self.task_wire(grant)
        accepted = self.begin(second, invocation)
        self.assertEqual((accepted["resultType"], accepted["status"], accepted["ttlMs"]), ("task", "working", 300000))
        handle = accepted["taskId"]
        self.assertTrue(handle.startswith("mcp-task-"))
        self.assertNotEqual(handle, task_id)
        self.assertEqual(self.begin(second, invocation)["taskId"], handle)
        before = n.get(task_id)
        self.assertEqual(before["value"]["lifecycle"], "Queued")
        self.assertEqual(self.task_call(second, "tasks/get", {"taskId": handle}, capability=False)["error"]["code"], -32021)
        updated = self.task_call(second, "tasks/update", {"taskId": handle,
                     "inputResponses": {"unissued": {"action": "accept", "content": {"ignored": True}}}})["result"]
        self.assertEqual(set(updated), {"resultType", "_meta"})
        self.assertEqual(updated["resultType"], "complete")
        self.assertEqual(n.get(task_id), before)
        self.assertEqual(self.task_call(second, "tasks/update", {"taskId": handle, "inputResponses": []})["error"]["code"], -32602)
        self.assertEqual(self.task_call(second, "tasks/list")["error"]["code"], -32601)

        wrong = self.task_wire(grant, endpoint="another-endpoint")
        other, other_grant = n.agent(["task.submit", "task.read", "system.status"])
        other_token = n.base / "other.token"
        with os.fdopen(os.open(other_token, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as file:
            file.write(other["token"])
        foreign = self.task_wire(other_grant, token_file=other_token)
        alternate = self.task_wire(self.grant([self.profile]))
        for refused in (wrong, foreign, alternate):
            denied = self.task_call(refused, "tasks/get", {"taskId": handle})["error"]
            self.assertEqual(denied["code"], -32602)
            self.assertEqual(self.task_call(refused, "tasks/cancel", {"taskId": handle})["error"], denied)
        self.assertEqual(self.tasks(), [original])

        self.scheduling("Enabled")
        settled = self.wait_status(second, handle)
        report = self.check_receipt(settled["result"], "system.status", grant)
        self.assertEqual(report["task_id"], task_id)
        self.assertEqual(report["result"]["kernel_release"], os.uname().release)
        second.close()
        n.stop(signal.SIGKILL)
        n.start()
        last = self.task_wire(grant)
        self.assertEqual(self.wait_status(last, handle), settled)
        self.assertEqual(self.begin(last, invocation)["taskId"], handle)
        self.assertEqual(len(self.tasks()), 1)
        balance = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((balance["reserved"], balance["spent"]), (0, 1))

        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assertEqual(self.task_call(last, "tasks/get", {"taskId": handle})["error"]["code"], -32602)
        denied_replay = self.begin(last, invocation)
        self.assertEqual(denied_replay["structuredContent"], {"code": "PermissionDenied"})
        self.assertEqual(len(self.tasks()), 1)

    def test_cancel_changes_the_original_queue_and_unnegotiated_call_stays_synchronous(self):
        n = self.node
        grant = self.grant([self.profile], units=3)
        client = self.task_wire(grant)
        disabled = self.task_wire(grant, endpoint=None)
        self.assertNotIn("extensions", self.task_call(disabled, "server/discover")["result"]["capabilities"])
        self.assertEqual(self.task_call(disabled, "tasks/get", {"taskId": "no-handle"})["error"]["code"], -32601)
        direct = self.task_call(client, "tools/call", {"name": "system.status"})["result"]
        self.assertEqual(direct["resultType"], "complete")
        self.check_receipt(direct, "system.status", grant)
        self.scheduling("Cordoned")
        [item] = self.tasks()
        accepted = self.begin(client, key())
        handle = accepted["taskId"]
        [original] = [entry for entry in self.tasks() if entry["resource_id"] != item["resource_id"]]
        cancelled = self.task_call(client, "tasks/cancel", {"taskId": handle})["result"]
        self.assertEqual(set(cancelled), {"resultType", "_meta"})
        self.assertEqual(cancelled["resultType"], "complete")
        self.assertEqual(self.wait_status(client, handle, "cancelled")["status"], "cancelled")
        task = n.wait(original["resource_id"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Cancelled", "NotStarted"))
        self.assertEqual(task["result"]["artifact_refs"], [])
        self.assertEqual(self.task_call(client, "tasks/cancel", {"taskId": handle})["result"], cancelled)
        self.assertEqual(n.call("Task.Get", {"id": original["resource_id"]}), task)
        balance = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((balance["reserved"], balance["spent"]), (0, 1))

    def test_elapsed_handle_expiry_does_not_admit_again_after_cleanup(self):
        n = self.node
        grant = self.grant([self.profile])
        invocation = key()
        client = self.task_wire(grant, ttl=2)
        accepted = self.begin(client, invocation)
        handle = accepted["taskId"]
        self.assertEqual(accepted["ttlMs"], 2000)
        [original] = self.tasks()
        n.wait(original["resource_id"])
        n.stop(signal.SIGKILL)
        time.sleep(2.2)
        n.start()
        client = self.task_wire(grant, ttl=300)
        self.assertEqual(self.task_call(client, "tasks/get", {"taskId": handle})["error"]["code"], -32602)
        cutoff = time.monotonic() + 5
        while True:
            with closing(sqlite3.connect(n.state / "state.db")) as database:
                if database.execute("SELECT count(*) FROM mcp_task_handles WHERE handle=?", (handle,)).fetchone() == (0,):
                    break
            self.assertLess(time.monotonic(), cutoff)
            time.sleep(0.03)
        rejected = self.begin(client, invocation)
        self.assertEqual(rejected["structuredContent"], {"code": "IdempotencyKeyExpired"})
        self.assertEqual(len(self.tasks()), 1)
        self.assertEqual(n.call("Task.Get", {"id": original["resource_id"]})["lifecycle"], "Succeeded")

    def test_bound_tool_result_stays_with_its_original_volume_revision(self):
        n = self.node
        token = self.agent["token"]

        def grant_for(tool, targets):
            return n.call("Grant.Issue", {
                "principal_id": self.agent["principal_id"], "actions": ["task.submit", "task.read", tool],
                "tool_targets": targets, "units": 3, "ttl_seconds": 120,
            })

        create = grant_for("volume.create", ["local"])
        request = n.prepare("volume.create", {"name": "mcp-task-volume"}, token=token,
                            delegation=create["delegation_ref"]["resource_id"])
        created = n.wait(n.call("Task.Submit", request, token)["task_id"], autoapprove=True)
        self.assertEqual(created["lifecycle"], "Succeeded", created)
        [artifact] = created["result"]["artifact_refs"]
        volume = n.call("Artifact.Read", {"reference": artifact}, token)
        first = n.perform("volume.write", {**target(volume), "name": "sample", "content": "initial"})["volume_ref"]
        grant = grant_for("volume.usage", [first["resource_id"]])
        client = self.task_wire(grant, extra=("--volume-usage", first["resource_id"]))

        def begin_usage(invocation):
            accepted = self.task_call(client, "tools/call", {"name": "volume.usage"}, invocation=invocation)["result"]
            self.assertEqual(accepted["resultType"], "task", accepted)
            return accepted["taskId"]

        invocation = key()
        handle = begin_usage(invocation)
        result = self.check_receipt(self.wait_status(client, handle)["result"], "volume.usage", grant)
        self.assertEqual((result["result"]["volume_ref"], result["result"]["logical_file_bytes"]), (first, "7"))
        [original] = n.call("Task.Results", {"id": result["task_id"]}, token)["result"]["artifact_refs"]

        second = n.perform("volume.write", {**target(first), "name": "sample", "content": "new"})["volume_ref"]
        self.assertNotEqual(second, first)
        refused = self.task_call(client, "tasks/get", {"taskId": handle})
        self.assertEqual(refused["error"]["code"], -32602, refused)
        self.assertEqual(begin_usage(invocation), handle)
        self.assertEqual(n.call("Artifact.Read", {"reference": original}, token), result["result"])

        fresh = self.check_receipt(self.wait_status(client, begin_usage(key()))["result"], "volume.usage", grant)
        self.assertNotEqual(fresh["task_id"], result["task_id"])
        self.assertEqual((fresh["result"]["volume_ref"], fresh["result"]["logical_file_bytes"]), (second, "3"))

    def test_bound_directory_input_and_page_resume_after_restart(self):
        n = self.node
        volume = self.create_owned_volume("mcp-directory-pages")
        volume_id = volume["resource_id"]
        directory = n.state / "volumes" / volume_id / "data" / "reports"
        directory.mkdir()
        (directory / "a").write_bytes(b"a")
        (directory / "b").write_bytes(b"bb")

        tool = "volume.files"
        grant = self.grant([volume_id], units=3, tools=[tool])
        client = self.task_wire(grant, extra=("--volume-files", volume_id))
        invocation = key()
        query = {"path": "reports", "limit": 1}

        def start(wire, arguments, invocation):
            return self.task_call(wire, "tools/call", {"name": tool, "arguments": arguments},
                                  invocation=invocation)["result"]

        handle = start(client, query, invocation)["taskId"]
        result = self.wait_status(client, handle)
        first = self.check_receipt(result["result"], tool, grant)
        revision = first["result"]["directory_revision"]
        self.assertRegex(revision, r"^sha256:[0-9a-f]{64}$")
        self.assertEqual(first["result"], {"entries": [{"name": "a", "kind": "file", "bytes": "1"}],
                                           "next_after": "a", "volume_version": volume["version"], "directory_revision": revision})
        client.close()
        n.stop(signal.SIGKILL)
        checked = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(checked["database_format"], 141)
        self.assertEqual(checked["sqlite_integrity"], "Verified")
        n.start()
        resumed = self.task_wire(grant, extra=("--volume-files", volume_id))
        self.assertEqual(start(resumed, query, invocation)["taskId"], handle)
        self.assertEqual(start(resumed, {**query, "path": ""}, invocation)["structuredContent"],
                         {"code": "IdempotencyConflict"})
        self.assertEqual(self.wait_status(resumed, handle), result)
        next_page = {**query, "after": "a", "volume_version": volume["version"], "directory_revision": revision}
        next_key = key()
        next_handle = start(resumed, next_page, next_key)["taskId"]
        next_result = self.check_receipt(self.wait_status(resumed, next_handle)["result"], tool, grant)
        self.assertEqual(next_result["result"],
                         {"entries": [{"name": "b", "kind": "file", "bytes": "2"}],
                          "next_after": None, "volume_version": volume["version"], "directory_revision": revision})
        self.assertNotEqual(next_result["task_id"], first["task_id"])
        (directory / "0").touch()
        resumed.close()
        n.stop(signal.SIGKILL)
        n.start()
        resumed = self.task_wire(grant, extra=("--volume-files", volume_id))
        self.assertEqual(start(resumed, next_page, next_key)["taskId"], next_handle)
        self.assertEqual(self.wait_status(resumed, next_handle)["result"]["structuredContent"], next_result)
        stale_handle = start(resumed, next_page, key())["taskId"]
        stale = self.wait_status(resumed, stale_handle)["result"]["structuredContent"]
        self.assertEqual(stale["code"], "Conflict", stale)
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assertEqual(self.task_call(resumed, "tasks/get", {"taskId": handle})["error"]["code"], -32602)

    @unittest.skipUnless(os.getenv("PSIOS_DIRECTORY_PREVIOUS_BIN_DIR"), "requires genuine format 147 binaries")
    def test_format_147_directory_handle_preserves_original_input_and_result(self):
        node = Node(os.environ["PSIOS_DIRECTORY_PREVIOUS_BIN_DIR"])
        self.addCleanup(node.close)
        agent = node.call("Agent.Register", {"name": "directory-upgrade"})
        token = agent["token"]

        def delegation(tool, scope):
            grant = node.call("Grant.Issue", {"principal_id": agent["principal_id"], "tool_targets": [scope],
                "actions": ["task.submit", "task.read", tool], "units": 4, "ttl_seconds": 300})
            return grant["delegation_ref"]["resource_id"]

        create = node.prepare("volume.create", {"name": "archive"}, token=token,
                              delegation=delegation("volume.create", "local"))
        created = node.call("Task.Submit", create, token)["task_id"]
        task = node.wait(created, autoapprove=True)
        volume = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)
        directory = node.state / "volumes" / volume["resource_id"] / "data"
        for name in ("a", "b"):
            (directory / name).touch()
        binding = {"tool": "volume.files", "target_id": volume["resource_id"], "query": {"limit": 1}}
        scope = {"endpoint": "directory", "delegation_id": delegation("volume.files", volume["resource_id"])}
        start = {**scope, "binding": binding, "invocation_key": key(), "ttl_seconds": 300}
        handle = node.call("McpTask.Start", start, token)["taskId"]
        page_task, = [r["resource_id"] for r in node.call("Resource.List", {"kind": "Task"})["items"]
                      if r["resource_id"] != created]
        self.assertEqual(node.wait(page_task)["lifecycle"], "Succeeded")
        access = {**scope, "bindings": [binding], "handle": handle}
        original = node.call("McpTask.Get", access, token)
        self.assertEqual(original["tool_result"]["result"]["next_after"], "a")
        self.assertNotIn("directory_revision", original["tool_result"]["result"])

        node.stop(signal.SIGKILL)
        old = node.daemon.copy()
        self.assertEqual(json.loads(subprocess.check_output([*old, "verify"]))["database_format"], 147)
        node.daemon[0] = str(self.node.binaries / "psiosd")
        node.start()
        self.assertEqual(node.call("McpTask.Start", start, token)["taskId"], handle)
        self.assertEqual(node.call("McpTask.Get", access, token), original)
        fresh = node.call("McpTask.Start", {**start, "invocation_key": key()}, token)["taskId"]
        new_task, = [r["resource_id"] for r in node.call("Resource.List", {"kind": "Task"})["items"]
                     if r["resource_id"] not in (created, page_task)]
        self.assertEqual(node.wait(new_task)["lifecycle"], "Succeeded")
        page = node.call("McpTask.Get", {**access, "handle": fresh}, token)["tool_result"]["result"]
        self.assertRegex(page["directory_revision"], r"^sha256:[0-9a-f]{64}$")
        self.assertEqual(page["entries"], original["tool_result"]["result"]["entries"])
        node.stop()
        before = (node.witness / "head").read_bytes()
        blocked = subprocess.run([*old, "serve"], capture_output=True, timeout=10)
        self.assertNotEqual(blocked.returncode, 0)
        self.assertIn(b"SchemaVersionUnsupported", blocked.stderr)
        self.assertEqual((node.witness / "head").read_bytes(), before)

    def test_bound_file_chunk_reuses_the_original_queued_task_after_restart(self):
        n = self.node
        volume = self.create_owned_volume("mcp-file-chunk-recovery")
        volume_id = volume["resource_id"]
        root = n.state / "volumes" / volume_id / "data"
        (root / "report").write_bytes(b"\x00\xff\x01")
        tool = "volume.read_chunk"
        grant = self.grant([volume_id], units=1, tools=[tool])
        client = self.task_wire(grant, extra=("--volume-read-chunk", volume_id))
        invocation = key()
        query = {"path": "report", "limit": 2, "volume_version": volume["version"]}

        def start(wire, arguments):
            return self.task_call(wire, "tools/call", {"name": tool, "arguments": arguments},
                                  invocation=invocation)["result"]

        self.scheduling("Cordoned")
        accepted = start(client, query)
        self.assertEqual(accepted["status"], "working", accepted)
        handle = accepted["taskId"]
        client.close()
        n.stop(signal.SIGKILL)
        checked = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(checked["database_format"], 142)
        self.assertEqual(checked["sqlite_integrity"], "Verified")
        n.start()
        resumed = self.task_wire(grant, extra=("--volume-read-chunk", volume_id))
        self.assertEqual(start(resumed, query)["taskId"], handle)
        refused = start(resumed, {**query, "limit": 1})
        self.assertEqual(refused["structuredContent"], {"code": "IdempotencyConflict"})
        self.scheduling("Enabled")
        settled = self.check_receipt(self.wait_status(resumed, handle)["result"], tool, grant)
        chunk = settled["result"]
        self.assertEqual((chunk["data_base64"], chunk["next_offset"], chunk["eof"]), ("AP8=", 2, False))
        self.assertEqual(chunk["volume_version"], volume["version"])
        self.assertEqual(start(resumed, query)["taskId"], handle)
        budget = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((budget["reserved"], budget["spent"]), (0, 1))
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        self.assertEqual(self.task_call(resumed, "tasks/get", {"taskId": handle})["error"]["code"], -32602)

    def test_retained_handle_limit_follows_the_agent_across_endpoints(self):
        n = self.node
        self.scheduling("Cordoned")
        grant = self.grant([self.profile], units=40)
        token = self.agent["token"]

        def start(agent_token, credential, endpoint, invocation):
            return n.call("McpTask.Start", {
                "endpoint": endpoint, "delegation_id": credential["delegation_ref"]["resource_id"],
                "invocation_key": invocation, "binding": {"tool": "system.status"}, "ttl_seconds": 300,
            }, agent_token)

        invocation = key()
        original = start(token, grant, "diagnostics", invocation)
        for attempt in range(31):
            start(token, grant, f"diagnostics-{attempt % 2}", key())
        limit = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((limit["reserved"], limit["spent"]), (32, 0))
        self.assertEqual(start(token, grant, "diagnostics", invocation), original)
        with self.assertRaises(Failure) as rejected:
            start(token, grant, "separate-endpoint", key())
        self.assertEqual(rejected.exception.code, "QuotaExceeded")
        self.assertTrue(rejected.exception.args[0]["retryable"])
        task_query = {"kind": "Task", "limit": 64}
        self.assertEqual(len(n.call("Resource.List", task_query)["items"]), 32)
        self.assertEqual(n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}), limit)

        other, other_grant = n.agent(["task.submit", "task.read", "system.status"])
        additional = start(other["token"], other_grant, "diagnostics", invocation)
        self.assertNotEqual(additional["taskId"], original["taskId"])
        self.assertEqual(len(n.call("Resource.List", task_query)["items"]), 33)

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_MCP_PREVIOUS_BIN_DIR"), "requires genuine format 121 binaries")
    def test_format_121_task_receipt_survives_migration_and_old_writer_is_fenced(self):
        previous = Path(os.environ["PSIOS_TEST_MCP_PREVIOUS_BIN_DIR"])
        node = Node(previous)
        self.addCleanup(node.close)
        task = node.submit("system.status", {})
        body = node.wait(task)
        [artifact] = body["result"]["artifact_refs"]
        receipt = node.call("Artifact.Read", {"reference": artifact})
        [operation] = body["operation_refs"]
        old = node.daemon.copy()
        node.stop(signal.SIGKILL)
        node.daemon[0] = str(self.node.binaries / "psiosd")
        before = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertEqual((before["database_format"], before["migration_required"]), (121, True))
        node.start()
        self.assertEqual(node.call("Task.Get", {"id": task}), body)
        self.assertEqual(node.call("Resource.Resolve", {"reference": operation})["reference"], operation)
        self.assertEqual(node.call("Artifact.Read", {"reference": artifact}), receipt)
        node.stop()
        after = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(after["database_format"], 122)
        self.assertFalse(after["migration_required"])
        blocked = subprocess.run([*old, "serve"], capture_output=True, timeout=10)
        self.assertNotEqual(blocked.returncode, 0)
        self.assertIn(b"SchemaVersionUnsupported", blocked.stderr)

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_MCP_TASKS_SDK_DIR"), "requires pinned official TypeScript Tasks SDK")
    def test_official_typescript_tasks_client(self):
        tools = ("system.status", "node.devices")
        grant = self.grant([self.profile, self.node_id], tools=tools)
        node = os.environ.get("PSIOS_TEST_MCP_TASKS_NODE", "node")
        harness = Path(__file__).with_name("mcp_tasks_sdk.mjs")
        adapter = [*self.adapter_command(grant), "--protocol", PROFILE, "--tasks-endpoint", "sdk-verification",
                   "--tool", tools[0], "--tool", tools[1]]
        for tool, query in zip(tools, ({}, {"limit": 1}), strict=True):
            result = json.loads(subprocess.check_output([node, harness, os.environ["PSIOS_TEST_MCP_TASKS_SDK_DIR"],
                                                         tool, json.dumps(query), *adapter], timeout=20))
            reported = self.check_receipt(result, tool, grant)["result"]
            if query:
                self.assertLessEqual(len(reported["devices"]), 1)
                self.assertTrue(reported["snapshot_digest"].startswith("sha256:"))
        self.assertEqual(len(self.tasks()), len(tools))
