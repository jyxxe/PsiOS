"""Modern MCP requests over an actual subprocess and authenticated PsiOS Core."""
import fcntl
import importlib.metadata
import os
import time
import unittest

from support import target
from test_mcp_stdio import McpFixture


PROFILE = "2026-07-28"
VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
CAPABILITIES_KEY = "io.modelcontextprotocol/clientCapabilities"
CLIENT_INFO = "io.modelcontextprotocol/clientInfo"
SERVER_INFO = "io.modelcontextprotocol/serverInfo"


def params(fields=None, version=PROFILE):
    return {"_meta": {VERSION_KEY: version, CAPABILITIES_KEY: {}}, **(fields or {})}


def send(client, request_id, method, arguments):
    client.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": arguments})


def response(client, request_id):
    frame = client.receive()
    assert frame["jsonrpc"] == "2.0" and frame["id"] == request_id, frame
    assert ("result" in frame) != ("error" in frame), frame
    return frame


def request(client, request_id, method, arguments):
    send(client, request_id, method, arguments)
    return response(client, request_id)


class ModernMcpStdioTests(McpFixture, unittest.TestCase):
    def command(self, grant):
        return [*super().command(grant), "--protocol", PROFILE]

    def assert_completed(self, frame, *, cacheable):
        result = frame["result"]
        self.assertEqual(result["resultType"], "complete")
        self.assertEqual(result["_meta"][SERVER_INFO]["name"], "psios-mcp")
        self.assertTrue(result["_meta"][SERVER_INFO]["version"])
        if cacheable:
            self.assertEqual((result["ttlMs"], result["cacheScope"]), (0, "private"))
        else:
            self.assertNotIn("ttlMs", result)
            self.assertNotIn("cacheScope", result)
        return result

    def test_direct_and_discovered_requests_are_versioned_and_scoped_independently(self):
        status = self.grant([self.profile])
        pressure = self.grant([self.node_id])
        client = self.wire(status)

        # A direct tool request needs no earlier discovery, session, or clientInfo.
        report = self.assert_completed(request(client, "reusable", "tools/call", params({"name": "system.status"})),
                                       cacheable=False)
        settled = self.check_receipt(report, "system.status", status)
        self.assertEqual(settled["result"]["kernel_release"], os.uname().release)
        self.assertEqual(len(self.tasks()), 1)

        discovered = self.assert_completed(request(client, "reusable", "server/discover", params()), cacheable=True)
        self.assertEqual(discovered["supportedVersions"], [PROFILE])
        self.assertEqual(discovered["capabilities"], {"tools": {"listChanged": False}})
        [listed] = self.assert_completed(request(client, 1, "tools/list", params()), cacheable=True)["tools"]
        self.assertEqual(listed["name"], "system.status")
        self.assertEqual(listed["inputSchema"], {"type": "object", "additionalProperties": False})
        self.assertNotIn("execution", listed)

        self.assertEqual(request(client, 2, "tools/list", {})["error"]["code"], -32602)
        self.assertEqual(request(client, 3, "tools/list", {"_meta": {VERSION_KEY: PROFILE}})["error"]["code"], -32602)
        self.assertEqual(request(client, 4, "tools/list", {"_meta": {VERSION_KEY: PROFILE,
            CAPABILITIES_KEY: []}})["error"]["code"], -32602)
        unsupported = request(client, 5, "tools/call", params({"name": "system.status"}, version="2025-11-25"))["error"]
        self.assertEqual(unsupported, {"code": -32022, "message": "Unsupported protocol version",
            "data": {"requested": "2025-11-25", "supported": [PROFILE]}})
        self.assertEqual(request(client, 6, "initialize", {"protocolVersion": PROFILE})["error"]["code"], -32601)
        self.assertEqual(request(client, 7, "ping", params())["error"]["code"], -32601)
        self.assertEqual(request(client, 8, "tools/call", params({"name": "system.status", "task": {}}))["error"]["code"], -32602)
        self.assertEqual(request(client, 9, "tools/call", params({"name": "system.status",
            "requestState": "untrusted"}))["error"]["code"], -32602)
        self.assertEqual(request(client, 10, "tools/call", params({"name": "volume.write"}))["error"]["code"], -32602)
        denied_params = params({"name": "node.pressure"})
        denied_params["_meta"][CLIENT_INFO] = {"name": "local-owner", "version": "forged"}
        denied = self.assert_completed(request(client, 11, "tools/call", denied_params), cacheable=False)
        self.assertTrue(denied["isError"])
        self.assertEqual(denied["structuredContent"], {"code": "PermissionDenied"})
        self.assertEqual(len(self.tasks()), 1)

        self.node.call("Grant.Revoke", target(status["grant_ref"]))
        self.assertEqual(self.assert_completed(request(client, 12, "tools/list", params()), cacheable=True)["tools"], [])
        self.assertEqual(self.node.get(settled["task_id"])["value"]["lifecycle"], "Succeeded")
        other = self.wire(pressure)
        [listed] = self.assert_completed(request(other, "reusable", "tools/list", params()), cacheable=True)["tools"]
        self.assertEqual(listed["name"], "node.pressure")
        sample = self.assert_completed(request(other, 1, "tools/call", params({"name": "node.pressure"})), cacheable=False)
        report = self.check_receipt(sample, "node.pressure", pressure)
        self.assertEqual(report["result"]["node_ref"]["resource_id"], self.node_id)
        self.assertEqual(report["result"]["kernel"]["source"], "LinuxProcfs")
        self.assertEqual(len(self.tasks()), 2)

    def test_exact_inflight_transport_cancellation_releases_the_original_queued_task_without_reply(self):
        n = self.node
        grant = self.grant([self.profile])
        client = self.wire(grant)
        node = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
        request_id = "7"
        send(client, request_id, "tools/call", params({"name": "system.status"}))
        limit = time.monotonic() + 5
        while not (tasks := self.tasks()):
            self.assertLess(time.monotonic(), limit, "tool call never reached the real local queue")
            time.sleep(0.02)
        [task] = tasks
        task_id = task["resource_id"]
        before = n.call("Task.Results", {"id": task_id}, self.agent["token"])
        self.assertEqual(before["lifecycle"], "Queued")
        self.assertEqual(before["task_revision"], task["version"])

        def cancel(identifier):
            client.send({"jsonrpc": "2.0", "method": "notifications/cancelled",
                         "params": {"requestId": identifier, "reason": "independent client stopped waiting"}})

        cancel(7)  # Integer and string IDs are distinct.
        send(client, request_id, "tools/call", params({"name": "system.status"}))
        busy = request(client, "busy", "tools/list", params())
        self.assertEqual(busy["error"]["code"], -32009)
        self.assertEqual(n.call("Task.Results", {"id": task_id}, self.agent["token"]), before)
        self.assertEqual(self.tasks(), [task])
        cancel(request_id)
        settled = n.wait(task_id)
        self.assertEqual((settled["lifecycle"], settled["outcome_knowledge"]), ("Cancelled", "NotStarted"))
        self.assertEqual(settled["result"]["artifact_refs"], [])
        after = n.call("Task.Results", {"id": task_id}, self.agent["token"])
        self.assertNotEqual(after["task_revision"], before["task_revision"])
        self.assertEqual(after["lifecycle"], "Cancelled")

        # Each probe must receive its own reply; a response for the abandoned
        # tool request would fail the independent client's correlation check.
        deadline = time.monotonic() + 5
        for index in range(100):
            probe = request(client, f"after-cancel-{index}", "tools/list", params())
            if "result" in probe:
                self.assert_completed(probe, cacheable=True)
                break
            self.assertEqual(probe["error"]["code"], -32009)
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)
        else:
            self.fail("cancelled request did not release its STDIO slot")
        self.assert_completed(request(client, request_id, "tools/list", params()), cacheable=True)
        self.assertEqual(len(self.tasks()), 1)
        budget = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((budget["reserved"], budget["spent"]), (0, 0))

    def test_uncorrelated_errors_and_output_backpressure_are_bounded(self):
        grant = self.grant([self.profile])
        client = self.wire(grant)
        client.process.stdin.write(b'{"unclosed":\n')
        invalid = client.receive()
        self.assertEqual(invalid["error"]["code"], -32700)
        self.assertNotIn("id", invalid)
        client.send({"jsonrpc": "2.0", "id": None, "method": "tools/list", "params": params()})
        invalid = client.receive()
        self.assertEqual(invalid["error"]["code"], -32600)
        self.assertNotIn("id", invalid)

        # Fill a small real stdout pipe while never leaving more than 1 KiB
        # of traffic in stdin. The server must exit without an EOF or reader.
        blocked = self.wire(grant)
        self.assertEqual(fcntl.fcntl(blocked.process.stdout, fcntl.F_SETPIPE_SZ, 4096), 4096)
        blocked.process.stdin.write(b'bad\n' * 250)
        self.assertNotEqual(blocked.process.wait(timeout=5), 0)
        self.assertEqual(self.tasks(), [])

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_MCP_MODERN_SDK") == "1", "requires official MCP SDK 2.0.0")
    def test_official_python_modern_stdio_client(self):
        import asyncio
        from mcp.client import Client
        from mcp.client.stdio import StdioServerParameters, stdio_client

        self.assertEqual(importlib.metadata.version("mcp"), "2.0.0")
        grant = self.grant([self.profile], units=2)
        command, *args = self.command(grant)

        async def invoke(mode):
            transport = stdio_client(StdioServerParameters(command=command, args=args, cwd=self.node.base))
            async with Client(transport, mode=mode, cache=None) as client:
                self.assertEqual(client.protocol_version, PROFILE)
                if mode == "auto":
                    self.assertEqual(client.server_info.name, "psios-mcp")
                self.assertEqual([tool.name for tool in (await client.list_tools()).tools], ["system.status"])
                result = await client.call_tool("system.status", {})
                return result.model_dump(by_alias=True, exclude_none=True)

        for mode in ("auto", PROFILE):
            report = asyncio.run(asyncio.wait_for(invoke(mode), timeout=40))
            self.assertEqual(report["resultType"], "complete")
            self.check_receipt(report, "system.status", grant)
        self.assertEqual(len(self.tasks()), 2)
