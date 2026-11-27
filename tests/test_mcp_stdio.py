"""Independent MCP JSON-RPC client exercising a real local PsiOS authority."""
from contextlib import closing
import importlib.metadata
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, target


PROFILE = "2025-11-25"
TOOLS = ["system.status", "node.pressure"]


class Wire:
    def __init__(self, command):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, bufsize=0, env={"PATH": "/usr/bin:/bin"})
        self.id = 0
        self.pending = b""

    def close(self):
        try:
            self.process.stdin.close()
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            raise
        finally:
            self.process.stdout.close()
            self.process.stderr.close()

    def send(self, frame):
        data = json.dumps(frame, separators=(",", ":")).encode() + b"\n"
        self.process.stdin.write(data)

    def receive(self):
        deadline = time.monotonic() + 35
        while b"\n" not in self.pending:
            left = deadline - time.monotonic()
            if left <= 0 or not select.select([self.process.stdout], [], [], left)[0]:
                raise AssertionError("MCP response did not arrive")
            data = os.read(self.process.stdout.fileno(), 64 * 1024)
            if not data:
                raise AssertionError("MCP server closed without responding")
            self.pending += data
            if len(self.pending) > 64 * 1024:
                raise AssertionError("oversized MCP response")
        body, self.pending = self.pending.split(b"\n", 1)
        return json.loads(body)

    def request(self, method, params=None):
        self.id += 1
        frame = {"jsonrpc": "2.0", "id": self.id, "method": method}
        if params is not None:
            frame["params"] = params
        self.send(frame)
        response = self.receive()
        assert response["jsonrpc"] == "2.0" and response["id"] == self.id, response
        assert ("result" in response) != ("error" in response), response
        return response

    def initialize(self, offered=PROFILE):
        result = self.request("initialize", {"protocolVersion": offered, "capabilities": {},
            "clientInfo": {"name": "local-owner", "version": "independent-wire-client"}})["result"]
        assert result["protocolVersion"] == PROFILE, result
        assert result["capabilities"] == {"tools": {"listChanged": False}}, result
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def invoke(self, name):
        return self.request("tools/call", {"name": name, "arguments": {}})["result"]


class McpFixture:
    def setUp(self):
        self.node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.agent = self.node.call("Agent.Register", {"name": "stdio-mcp", "peer_uid": os.getuid()})
        self.token_file = self.node.base / "stdio.token"
        fd = os.open(self.token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as credential:
            credential.write(self.agent["token"])
        self.profile = "profile-local"
        self.node_id = self.node.call("Node.Get", {})["reference"]["resource_id"]

    def grant(self, targets, units=2, *, tools=TOOLS):
        return self.node.call("Grant.Issue", {
            "principal_id": self.agent["principal_id"], "actions": ["task.submit", "task.read", *tools],
            "tool_targets": targets, "units": units, "ttl_seconds": 120,
        })

    def adapter_command(self, grant):
        return [str(self.node.binaries / "psios-mcp"), "--socket", str(self.node.socket),
                "--token-file", str(self.token_file), "--delegation", grant["delegation_ref"]["resource_id"]]

    def command(self, grant):
        return [*self.adapter_command(grant), "--tool", "system.status", "--tool", "node.pressure"]

    def wire(self, grant):
        client = Wire(self.command(grant))
        self.addCleanup(client.close)
        return client

    def tasks(self):
        return self.node.call("Resource.List", {"kind": "Task"})["items"]

    def check_receipt(self, response, tool, grant):
        self.assertFalse(response["isError"], response)
        result = response["structuredContent"]
        self.assertEqual(json.loads(response["content"][0]["text"]), result)
        task = self.node.get(result["task_id"])
        self.assertEqual(task["resource"]["spec"]["task_type"], tool)
        self.assertEqual(task["resource"]["spec"]["delegation_ref"], grant["delegation_ref"])
        state = task["value"]
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"]), ("Succeeded", "Known"))
        self.assertTrue(state["current_run_id"])
        self.assertEqual(len(state["operation_refs"]), 1)
        self.assertEqual(self.node.call("Task.Get", {"id": result["task_id"]}, self.agent["token"]), state)
        [artifact] = state["result"]["artifact_refs"]
        self.assertEqual(self.node.call("Artifact.Read", {"reference": artifact}, self.agent["token"]), result["result"])
        return result


class McpStdioTests(McpFixture, unittest.TestCase):
    def test_wire_scope_revocation_budget_and_restart(self):
        status = self.grant([self.profile], units=1)
        pressure = self.grant([self.node_id])
        with self.assertRaises(Failure) as denied_owner:
            self.node.call("Capability.LocalReadTools", {
                "delegation_id": "owner-delegation", "tools": ["system.status"],
            })
        self.assertEqual(denied_owner.exception.code, "PermissionDenied")
        client = self.wire(status)
        self.assertEqual(client.request("ping")["result"], {})
        self.assertEqual(client.request("tools/list")["error"]["code"], -32002)
        client.initialize("2025-06-18")
        self.assertEqual(client.request("server/discover")["error"]["code"], -32601)
        [listed] = client.request("tools/list")["result"]["tools"]
        self.assertEqual(listed["name"], "system.status")
        self.assertEqual(listed["inputSchema"], {"type": "object", "additionalProperties": False})
        self.assertEqual(listed["execution"]["taskSupport"], "forbidden")
        self.assertEqual(self.tasks(), [])
        self.assertEqual(client.request("tools/call", {"name": "volume.write", "arguments": {}})["error"]["code"], -32602)
        self.assertEqual(client.request("tools/call", {"name": "system.status", "task": {}})["error"]["code"], -32602)
        malformed = client.request("tools/call", {"name": "system.status", "arguments": {"file": "/"}})["result"]
        self.assertEqual(malformed["structuredContent"], {"code": "InvalidArgument"})
        denied = client.invoke("node.pressure")
        self.assertTrue(denied["isError"])
        self.assertEqual(denied["structuredContent"], {"code": "PermissionDenied"})
        self.assertEqual(self.tasks(), [])
        with closing(sqlite3.connect(self.node.state / "state.db")) as database:
            self.assertEqual(database.execute("SELECT count(*) FROM task_preparations").fetchone(), (0,))

        report = self.check_receipt(client.invoke("system.status"), "system.status", status)
        self.assertEqual(report["result"]["kernel_release"], os.uname().release)
        exhausted = client.invoke("system.status")
        self.assertTrue(exhausted["isError"], exhausted)
        self.assertEqual(exhausted["structuredContent"]["code"], "QuotaExceeded")
        self.assertEqual(len(self.tasks()), 1)
        self.node.stop(signal.SIGKILL)
        self.assertEqual(client.request("tools/list")["error"]["code"], -32001)
        self.node.start()
        self.assertEqual(self.node.get(report["task_id"])["value"]["lifecycle"], "Succeeded")
        self.node.call("Grant.Revoke", target(status["grant_ref"]))
        self.assertEqual(client.request("tools/list")["result"]["tools"], [])
        self.assertEqual(client.invoke("system.status")["structuredContent"], {"code": "PermissionDenied"})

        other = self.wire(pressure)
        other.initialize()
        self.assertEqual([tool["name"] for tool in other.request("tools/list")["result"]["tools"]], ["node.pressure"])
        sample = self.check_receipt(other.invoke("node.pressure"), "node.pressure", pressure)
        self.assertEqual(sample["result"]["node_ref"]["resource_id"], self.node_id)
        self.assertEqual(sample["result"]["kernel"]["source"], "LinuxProcfs")
        self.assertEqual(len(self.tasks()), 2)

    def test_unreadable_id_and_bounded_frame_follow_profile(self):
        client = self.wire(self.grant([self.profile]))
        client.process.stdin.write(b'{"unclosed":\n')
        invalid = client.receive()
        self.assertEqual(invalid["error"]["code"], -32700)
        self.assertNotIn("id", invalid)
        client.send({"jsonrpc": "2.0", "id": None, "method": "ping"})
        uncorrelated = client.receive()
        self.assertEqual(uncorrelated["error"]["code"], -32600)
        self.assertNotIn("id", uncorrelated)
        client.send({"jsonrpc": "2.0", "id": "original", "method": "tools/list"})
        self.assertEqual(client.receive()["id"], "original")
        client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.assertEqual(client.request("ping")["result"], {})
        frame = memoryview(b"x" * (64 * 1024) + b"\n")
        while frame:
            frame = frame[client.process.stdin.write(frame):]
        large = client.receive()
        self.assertEqual(large["error"]["code"], -32700)
        self.assertNotIn("id", large)
        self.assertEqual(client.process.wait(timeout=5), 0)
        self.assertEqual(self.tasks(), [])

    def test_operator_cancellation_and_stdio_disconnect_leave_real_task_receipts(self):
        n = self.node
        grant = self.grant([self.profile])
        client = self.wire(grant)
        client.initialize()

        def scheduling(mode):
            node = n.call("Node.Get", {})["reference"]
            n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})

        def queued(prior_ids):
            client.id += 1
            client.send({"jsonrpc": "2.0", "id": client.id, "method": "tools/call",
                         "params": {"name": "system.status"}})
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                new = [ref for ref in self.tasks() if ref["resource_id"] not in prior_ids]
                if new:
                    [reference] = new
                    self.assertEqual(n.call("Task.Get", {"id": reference["resource_id"]})["lifecycle"], "Queued")
                    return reference
                time.sleep(0.02)
            self.fail("client did not durably submit the read-only Task")

        scheduling("Cordoned")
        cancelled = queued(set())
        n.call("Task.Cancel", target(cancelled))
        result = client.receive()["result"]
        self.assertTrue(result["isError"])
        self.assertEqual(result["structuredContent"], {"task_id": cancelled["resource_id"], "code": "Cancelled"})
        detached = queued({cancelled["resource_id"]})
        client.process.kill()
        self.assertEqual(client.process.wait(timeout=5), -signal.SIGKILL)
        self.assertEqual(n.get(detached["resource_id"])["reference"], detached)
        self.assertEqual(n.call("Task.Get", {"id": detached["resource_id"]})["lifecycle"], "Queued")
        scheduling("Enabled")
        settled = n.wait(detached["resource_id"])
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Succeeded", "Known"))
        self.assertEqual(len(self.tasks()), 2)

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_MCP_SDK") == "1", "requires pinned official MCP SDK 1.28.1")
    def test_official_python_stdio_client(self):
        import asyncio
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client

        self.assertEqual(importlib.metadata.version("mcp"), "1.28.1")
        tools = ["system.status", "node.inventory"]
        grant = self.grant([self.profile, self.node_id], tools=tools)
        command, *args = [*self.adapter_command(grant), "--tool", tools[0], "--tool", tools[1]]

        async def invoke():
            async with stdio_client(StdioServerParameters(command=command, args=args, cwd=Path(self.node.base))) as pipes:
                async with ClientSession(*pipes) as session:
                    initialized = await session.initialize()
                    self.assertEqual(initialized.protocolVersion, PROFILE)
                    self.assertEqual({tool.name for tool in (await session.list_tools()).tools}, set(tools))
                    return [(await session.call_tool(tool, {})).model_dump(exclude_none=True) for tool in tools]

        reports = asyncio.run(asyncio.wait_for(invoke(), timeout=40))
        for tool, report in zip(tools, reports, strict=True):
            settled = self.check_receipt(report, tool, grant)
            self.assertEqual(settled["result"]["kernel_release"], os.uname().release)
        self.assertEqual(len(self.tasks()), len(tools))
