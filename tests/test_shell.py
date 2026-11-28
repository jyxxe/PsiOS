"""Exercise the local browser boundary against a real psiosd process."""
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from contextlib import closing
import hashlib
from http.client import HTTPConnection
import io
import base64
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import socket
import subprocess
import tarfile
import time
import unittest
from threading import Event
from urllib.parse import urlsplit

from model_delivery import frame
from test_model_fallback import publish_skill, register_provider
from support import Failure, Node, key, target


class Shell:
    def __init__(self, node, token_file, endpoint=None, *, cancel_pending=False, send_chat=False,
                 review_work_items=False, manage_agent_sessions=False, files=None, files_preview=False, resolver_delegation=None):
        self.process = subprocess.Popen(
            [node.binaries / "psiosctl", "--socket", endpoint or node.socket, "--token-file", token_file, "shell",
             *(["--cancel-pending-tasks"] if cancel_pending else []), *(["--send-chat"] if send_chat else []),
             *(["--review-work-items"] if review_work_items else []),
             *(["--manage-agent-sessions"] if manage_agent_sessions else []),
             *(["--files-volume", files[0], "--files-delegation", files[1]] if files else []),
             *(["--files-preview"] if files_preview else []),
             *(["--resolver-delegation", resolver_delegation] if resolver_delegation else [])],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
        )
        try:
            self.origin = self.line()
            self.code = self.line().removeprefix("Pairing code: ")
            address = urlsplit(self.origin)
            if (address.scheme != "http" or address.hostname != "127.0.0.1" or
                    address.path or address.query or address.fragment or not address.port or
                    len(self.code) != 32):
                raise AssertionError("Shell did not provide a loopback URL and separate pairing code")
            self.port = address.port
        except BaseException:
            self.close()
            raise

    def line(self):
        if not select.select([self.process.stdout], [], [], 10)[0]:
            raise AssertionError("Shell did not become ready")
        return self.process.stdout.readline().decode().strip()

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.communicate()
            raise

    def http(self, path, data=None, token=None, headers=None, method="POST", raw=None):
        request_headers = dict(headers or {})
        if method == "POST":
            request_headers.setdefault("Origin", self.origin)
            request_headers.setdefault("Content-Type", "application/json")
        if token is not None:
            request_headers["Authorization"] = "Bearer " + token
        connection = HTTPConnection("127.0.0.1", self.port, timeout=10)
        try:
            body = raw if raw is not None else json.dumps(data) if data is not None else None
            connection.request(method, path, body, request_headers)
            result = connection.getresponse()
            body = result.read()
            if result.getheader("Content-Type", "").startswith("application/json"):
                body = json.loads(body)
            return result.status, result.headers, body
        finally:
            connection.close()

    def pair(self):
        status, _, result = self.http("/api/session", {"code": self.code})
        if status != 200:
            raise AssertionError(f"Shell pairing failed with HTTP {status}")
        return result


class ShellTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def shell(self, token_file=None, *, cancel_pending=False, send_chat=False,
              review_work_items=False, manage_agent_sessions=False, files=None, files_preview=False, resolver_delegation=None):
        shell = Shell(self.node, token_file or self.node.witness / "owner.token",
                      cancel_pending=cancel_pending, send_chat=send_chat, review_work_items=review_work_items,
                      manage_agent_sessions=manage_agent_sessions, files=files, files_preview=files_preview,
                      resolver_delegation=resolver_delegation)
        self.addCleanup(shell.close)
        return shell

    def error(self, shell, status, code, path, **kwargs):
        actual, headers, body = shell.http(path, **kwargs)
        self.assertEqual(actual, status)
        self.assertEqual(body["error"]["code"], code)
        self.assertEqual(headers["Cache-Control"], "no-store")
        self.assertIsNone(headers["Access-Control-Allow-Origin"])

    def file_volume(self, *, preview=False):
        node = self.node
        volume = node.perform("volume.create", {"name": "shell-files"})
        volume_id = volume["resource_id"]
        root = node.state / "volumes" / volume_id / "data"
        documents = root / "documents"
        documents.mkdir()
        (documents / "会议记录.txt").write_text("private file contents")
        (root / "host-link").symlink_to("/etc/passwd")
        for index in range(36):
            (root / f"item-{index:02d}.txt").write_bytes(b"x" * index)
        owner = node.call("Principal.WhoAmI", {})["principal_id"]
        actions = ["task.submit", "task.read", "volume.files"]
        if preview:
            actions.append("volume.read_chunk")
        grant = node.call("Grant.Issue", {"principal_id": owner, "actions": actions,
                                        "tool_targets": [volume_id], "units": 20, "ttl_seconds": 180})
        return root, grant, (volume_id, grant["delegation_ref"]["resource_id"])

    def test_file_browser_uses_one_volume_and_exact_core_directory_revisions(self):
        root, grant, binding = self.file_volume()
        endpoint = "/api/volume-directory"

        def request(query=None):
            return {"key": key(), "query": query or {}}

        ordinary = self.shell()
        paired = ordinary.pair()
        self.assertNotIn("browse_files", paired)
        self.error(ordinary, 404, "NotFound", endpoint, data=request(), token=paired["token"])

        browser = self.shell(files=binding)
        self.error(browser, 401, "AuthenticationRequired", endpoint, data=request())
        paired = browser.pair()
        self.assertIs(paired["browse_files"], True)
        token = paired["token"]
        for params in (request({"target_id": "other-volume"}), request({"path": "../etc"}),
                       request({"path": "/etc"}), request({"after": "item-00.txt", "volume_version": "r1"})):
            self.error(browser, 400, "InvalidArgument", endpoint, data=params, token=token)
        self.error(browser, 503, "StorageUnavailable", endpoint, data=request({"path": "host-link"}), token=token)
        first_request = request()
        status, headers, first = browser.http(endpoint, first_request, token)
        self.assertEqual((status, headers["Cache-Control"]), (200, "no-store"))
        self.assertEqual(len(first["entries"]), 32)
        self.assertEqual(first["entries"][:2], [
            {"name": "documents", "kind": "directory", "bytes": first["entries"][0]["bytes"]},
            {"name": "host-link", "kind": "unsupported", "bytes": "0"},
        ])
        self.assertEqual(browser.http(endpoint, first_request, token)[2], first)
        continuation = dict(after=first["next_after"], volume_version=first["volume_version"],
                            directory_revision=first["directory_revision"])
        status, _, tail = browser.http(endpoint, request(continuation), token)
        self.assertEqual(status, 200, tail)
        self.assertEqual((len(tail["entries"]), tail["next_after"]), (6, None))
        self.assertEqual(tail["directory_revision"], first["directory_revision"])
        status, _, page = browser.http(endpoint, request({"path": "documents"}), token)
        self.assertEqual(status, 200, page)
        self.assertEqual(page["entries"], [{"name": "会议记录.txt", "kind": "file", "bytes": "21"}])
        (root / "added.txt").touch()
        self.error(browser, 409, "Conflict", endpoint, data=request(continuation), token=token)
        self.node.call("Grant.Revoke", target(grant["grant_ref"]))
        self.error(browser, 403, "PermissionDenied", endpoint, data=request(), token=token)
        self.error(browser, 403, "PermissionDenied", endpoint, data=first_request, token=token)
        self.assertEqual((root / "documents/会议记录.txt").read_text(), "private file contents")

    def test_file_preview_requires_explicit_opt_in_and_separate_read_scope(self):
        root, grant, binding = self.file_volume(preview=True)
        file = root / "documents/text.txt"
        file.write_bytes(b"x" * 6144 + b"hidden end")
        volume = self.node.get(binding[0])["reference"]
        endpoint = "/api/volume-preview"

        def input_for(path="documents/text.txt", **values):
            return {"key": key(), "path": path, "volume_version": volume["version"], **values}

        metadata = self.shell(files=binding)
        paired = metadata.pair()
        self.assertNotIn("preview_files", paired)
        self.error(metadata, 404, "NotFound", endpoint, data=input_for(), token=paired["token"])
        owner = self.node.call("Principal.WhoAmI", {})["principal_id"]
        list_only = self.node.call("Grant.Issue", {"principal_id": owner, "tool_targets": [binding[0]],
            "actions": ["task.submit", "task.read", "volume.files"], "units": 4, "ttl_seconds": 180})
        denied = self.shell(files=(binding[0], list_only["delegation_ref"]["resource_id"]), files_preview=True)
        self.error(denied, 403, "PermissionDenied", endpoint, data=input_for(), token=denied.pair()["token"])
        shell = self.shell(files=binding, files_preview=True)
        paired = shell.pair()
        token = paired["token"]
        self.assertIs(paired["preview_files"], True)
        for data in (input_for(path="../outside"), input_for(offset=20), input_for(target_id="other")):
            self.error(shell, 400, "InvalidArgument", endpoint, data=data, token=token)
        first = input_for()
        status, headers, result = shell.http(endpoint, first, token)
        self.assertEqual((status, headers["Cache-Control"]), (200, "no-store"))
        self.assertEqual((result["path"], result["offset"], result["next_offset"], result["eof"]),
                         ("documents/text.txt", 0, 6144, False))
        self.assertEqual(base64.b64decode(result["data_base64"]), b"x" * 6144)
        self.assertEqual(shell.http(endpoint, first, token)[2], result)
        self.node.call("Grant.Revoke", target(grant["grant_ref"]))
        self.error(shell, 403, "PermissionDenied", endpoint, data=first, token=token)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_previews_utf8_files_as_bounded_literal_text(self):
        root, _, binding = self.file_volume(preview=True)
        documents = root / "documents"
        sample = '<b id="preview-fixture">会议笔记</b>'
        (documents / "会议记录.txt").write_text(sample)
        (documents / "binary.bin").write_bytes(b"\xff\x80")
        (documents / "long.txt").write_bytes(b"q" * 6143 + "界".encode("utf-8"))
        shell = self.shell(files=binding, files_preview=True)
        args = dict(origin=shell.origin, code=shell.code, filePreview=True, sample=sample,
                    profile=str(self.node.base / "browser"))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=60)
        self.assertEqual((documents / "会议记录.txt").read_text(), sample)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_files_navigation_rechecks_its_grant_after_restoring(self):
        _, grant, binding = self.file_volume()
        shell = self.shell(files=binding)
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(45)
            args = dict(origin=shell.origin, code=shell.code, fileBrowser=True,
                        profile=str(self.node.base / "browser"), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    self.node.call("Grant.Revoke", target(grant["grant_ref"]))
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=45), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    def resolver_grant(self, scope):
        grant = self.node.call("Grant.Issue", {"principal_id": "local-owner",
            "actions": ["node.resolver", "task.submit", "task.read"], "tool_targets": [scope],
            "units": 3, "ttl_seconds": 120})
        return grant["delegation_ref"]["resource_id"], grant["grant_ref"]

    def resolver_fixture(self):
        fixture = Path(os.environ["PSIOS_TEST_RESOLVER_FIXTURE"])
        resolver = Path("/etc/resolv.conf")
        self.assertNotEqual(fixture.resolve(), resolver.resolve())
        self.assertTrue(fixture.samefile(resolver))
        return fixture

    @unittest.skipUnless(os.getenv("PSIOS_TEST_RESOLVER_FIXTURE"), "requires a private resolver mount fixture")
    def test_resolver_observation_and_file_preview_keep_independent_scopes(self):
        self.resolver_fixture().write_text("nameserver 192.0.2.53\n")
        _, _, files = self.file_volume(preview=True)
        node = self.node.call("Node.Get", {})
        delegation, grant = self.resolver_grant(node["reference"]["resource_id"])
        shell = self.shell(files=files, files_preview=True, resolver_delegation=delegation)
        pair = shell.pair()
        self.assertTrue(pair["observe_resolver"] and pair["browse_files"] and pair["preview_files"])
        token = pair["token"]
        resolver = "/api/node-resolver"
        status, _, report = shell.http(resolver, {}, token)
        self.assertEqual(status, 200, report)
        self.assertEqual(report["nameservers"], [{"address": "192.0.2.53", "zone": None}])
        self.node.call("Grant.Revoke", target(grant))
        self.error(shell, 403, "PermissionDenied", resolver, data={}, token=token)
        volume = self.node.get(files[0])["reference"]
        status, _, preview = shell.http("/api/volume-preview", {
            "key": key(), "path": "documents/会议记录.txt", "volume_version": volume["version"],
        }, token)
        self.assertEqual(status, 200, preview)
        self.assertEqual(base64.b64decode(preview["data_base64"]), b"private file contents")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_RESOLVER_FIXTURE"), "requires a private resolver mount fixture")
    def test_resolver_observation_requires_explicit_local_scope_and_reads_current_nameservers(self):
        fixture = self.resolver_fixture()
        route = "/api/node-resolver"
        default = self.shell()
        pair = default.pair()
        self.assertFalse(pair["observe_resolver"])
        self.error(default, 404, "NotFound", route, data={}, token=pair["token"])

        wrong, _ = self.resolver_grant("profile-local")
        refused = self.shell(resolver_delegation=wrong)
        self.error(refused, 403, "PermissionDenied", route, data={}, token=refused.pair()["token"])
        node = self.node.call("Node.Get", {})
        delegation, grant = self.resolver_grant(node["reference"]["resource_id"])
        with self.assertRaises(Failure) as discovery:
            self.node.call("Capability.LocalReadTools", {"delegation_id": delegation, "tools": ["node.resolver"]})
        self.assertEqual(discovery.exception.code, "PermissionDenied")
        shell = self.shell(resolver_delegation=delegation)
        self.error(shell, 401, "AuthenticationRequired", route, data={})
        pair = shell.pair()
        self.assertTrue(pair["observe_resolver"])
        bearer = pair["token"]
        self.error(shell, 400, "InvalidArgument", route, data={"path": "/etc/hosts"}, token=bearer)
        fixture.write_text("search private.invalid\nnameserver 192.0.2.42\nnameserver fe80::53%eth0\n")
        status, _, observed = shell.http(route, {}, bearer)
        self.assertEqual(status, 200, observed)
        self.assertEqual(set(observed), {"nameservers", "observed_at", "boot_id"})
        self.assertEqual(observed["boot_id"], node["state"]["boot_id"])
        self.assertEqual(observed["nameservers"], [{"address": "192.0.2.42", "zone": None},
                                                   {"address": "fe80::53", "zone": "eth0"}])
        fixture.write_text("nameserver 2001:db8::53\n")
        status, _, fresh = shell.http(route, {}, bearer)
        self.assertEqual((status, fresh["nameservers"]), (200, [{"address": "2001:db8::53", "zone": None}]))
        self.node.call("Grant.Revoke", target(grant))
        self.error(shell, 403, "PermissionDenied", route, data={}, token=bearer)
        self.assertEqual(shell.http("/api/node", {}, bearer)[0], 200)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_RESOLVER_FIXTURE"), "requires a private resolver mount fixture")
    def test_resolver_observation_rechecks_the_grant_before_releasing_the_artifact(self):
        self.resolver_fixture().write_text("nameserver 192.0.2.44\n")
        node = self.node
        delegation, grant = self.resolver_grant(node.call("Node.Get", {})["reference"]["resource_id"])
        proxy = node.base / "resolver-race.sock"
        waiting, release = Event(), Event()
        methods = ("Principal.WhoAmI", "Capability.CheckLocalReadTool", "Task.Prepare", "Task.Submit", "Task.Wait",
                   "Artifact.ReadChunk", "Capability.CheckLocalReadTool")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)
            forwarded = pool.submit(self.relay, listener, methods, "Artifact.ReadChunk", waiting, release)
            shell = Shell(node, node.witness / "owner.token", proxy, resolver_delegation=delegation)
            self.addCleanup(shell.close)
            read = pool.submit(shell.http, "/api/node-resolver", {}, shell.pair()["token"])
            try:
                self.assertTrue(waiting.wait(8))
                node.call("Grant.Revoke", target(grant))
            finally:
                release.set()
            forwarded.result(8)
            status, _, denied = read.result(8)
            self.assertEqual((status, denied["error"]["code"]), (403, "PermissionDenied"))
            self.assertNotIn("nameservers", denied)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_resolver_observation_clears_on_minimize_and_checks_revocation(self):
        node = self.node.call("Node.Get", {})
        delegation, grant = self.resolver_grant(node["reference"]["resource_id"])
        shell = self.shell(resolver_delegation=delegation)
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(20)
            args = dict(origin=shell.origin, code=shell.code, nodeResolver=True, node=node["reference"]["resource_id"],
                        boot=node["state"]["boot_id"], profile=str(self.node.base / "browser"),
                        control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    self.node.call("Grant.Revoke", target(grant))
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=35), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    def agent_sessions(self, count):
        node = self.node
        provider, model = register_provider(node, "shell-agent-runtime", strict=True)
        skill = publish_skill(node)

        def create(name, amount):
            actor = node.call("Agent.Register", {"name": name})
            grant = node.call("Grant.Issue", {"principal_id": actor["principal_id"],
                                              "actions": ["resource.read", "task.read", "session.run",
                                                          "task.submit", "system.status"],
                                              "model_calls": 12, "model_lease_ms": 240_000,
                                              "units": 12, "ttl_seconds": 180})
            sessions = [node.call("Session.Open", {"key": key(), "goal": "private runtime goal",
                                                   "model_ref": model, "skill_ref": skill,
                                                   "context_policy": "ReferencesOnly", "max_turns": 3,
                                                   "delegation_id": grant["delegation_ref"]["resource_id"]},
                                  actor["token"])["reference"]["resource_id"] for _ in range(amount)]
            return actor, grant["grant_ref"], sessions

        actor, grant, visible = create("shell-session-reader", count)
        _, _, hidden = create("other-session-owner", 1)
        identity = node.base / "runtime-reader.token"
        identity.touch(mode=0o600)
        identity.write_text(actor["token"])
        return identity, grant, actor, visible, hidden[0], provider

    def finish_agent_session(self, provider, session):
        lease = self.node.call("Model.Claim", {"session_id": session, "key": key()}, provider["token"])
        self.node.call("Model.Respond", {"session_id": session, "lease_id": lease["lease_id"],
                                         "decision": {"action": "finish", "summary": "private model summary"},
                                         "usage": {"input_tokens": 12, "output_tokens": 3}}, provider["token"])
        return lease["lease_id"]

    def test_agent_runtime_controls_require_opt_in_current_revision_and_authority(self):
        identity, grant, actor, sessions, hidden, provider = self.agent_sessions(1)
        node = self.node
        session = sessions[0]
        params = {"id": session}
        before = node.call("Session.Get", params, actor["token"])["reference"]
        request = {"action": "pause", "reference": before}
        endpoint = "/api/control-agent-session"
        readonly = self.shell(identity)
        paired = readonly.pair()
        self.assertFalse(paired["manage_agent_sessions"])
        self.error(readonly, 404, "NotFound", endpoint, data=request, token=paired["token"])

        shell = self.shell(identity, manage_agent_sessions=True)
        paired = shell.pair()
        self.assertTrue(paired["manage_agent_sessions"])
        token = paired["token"]
        self.error(shell, 401, "AuthenticationRequired", endpoint, data=request)
        self.error(shell, 400, "InvalidArgument", endpoint, data=dict(request, action="cancel"), token=token)
        other = node.call("Session.Get", {"id": hidden})["reference"]
        self.error(shell, 403, "PermissionDenied", endpoint, data=dict(request, reference=other), token=token)
        self.error(shell, 409, "Conflict", endpoint, data=dict(request, reference=dict(before, digest="sha256:" + "a" * 64)),
                   token=token)
        code, _, receipt = shell.http(endpoint, request, token)
        self.assertEqual(code, 200)
        paused = node.call("Session.Get", params, actor["token"])
        self.assertEqual(receipt, {"reference": paused["reference"]})
        self.assertEqual(paused["session"]["pause_reason"], "OperatorPaused")
        self.error(shell, 409, "Conflict", endpoint, data=request, token=token)
        code, _, receipt = shell.http(endpoint, {"action": "resume", "reference": paused["reference"]}, token)
        self.assertEqual(code, 200)
        resumed = node.call("Session.Get", params, actor["token"])
        self.assertEqual(receipt, {"reference": resumed["reference"]})
        self.assertEqual(resumed["session"]["lifecycle"], "AwaitingModel")
        lease = node.call("Model.Claim", {"session_id": session, "key": key()}, provider["token"])
        unknown = node.call("Model.ReportFailure", {"session_id": session, "lease_id": lease["lease_id"],
                                                   "model_ref": lease["context"]["model_ref"], "outcome": "Unknown"},
                            provider["token"])
        resume = {"action": "resume", "reference": unknown["session_ref"]}
        self.error(shell, 409, "PreconditionFailed", endpoint, data=resume, token=token)
        node.call("Grant.Revoke", target(grant))
        self.error(shell, 403, "PermissionDenied", endpoint, data=resume, token=token)
        self.assertEqual(node.call("Session.Get", params)["reference"], unknown["session_ref"])

    def test_agent_runtime_pause_does_not_interrupt_a_concurrently_claimed_model(self):
        identity, _, actor, sessions, _, provider = self.agent_sessions(1)
        node = self.node
        session = sessions[0]
        params = {"id": session}
        reference = node.call("Session.Get", params, actor["token"])["reference"]
        endpoint = node.base / "session-control.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(endpoint))
            listener.settimeout(8)
            listener.listen()
            relay = pool.submit(self.relay, listener, ("Principal.WhoAmI", "Session.Get", "Session.Pause"),
                                "Session.Get", waiting, release)
            shell = Shell(node, identity, endpoint, manage_agent_sessions=True)
            self.addCleanup(shell.close)
            token = shell.pair()["token"]
            pause = pool.submit(shell.http, "/api/control-agent-session", {"reference": reference, "action": "pause"}, token)
            try:
                self.assertTrue(waiting.wait(8))
                lease = node.call("Model.Claim", {"session_id": session, "key": key()}, provider["token"])
            finally:
                release.set()
            relay.result(8)
            code, _, result = pause.result(8)
            self.assertEqual((code, result["error"]["code"]), (409, "Conflict"))
        state = node.call("Session.Get", params, actor["token"])["session"]
        self.assertEqual((state["lifecycle"], state["current_lease"]), ("ModelInFlight", lease["lease_id"]))

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_observes_paused_agent_session_when_the_control_receipt_is_lost(self):
        identity, _, actor, sessions, _, _ = self.agent_sessions(1)
        node = self.node
        session = sessions[0]
        path = node.base / "session-control.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor() as pool:
            listener.bind(str(path))
            listener.settimeout(12)
            listener.listen()
            methods = ("Principal.WhoAmI", "Resource.List", "Session.Get", "Session.Get", "Session.ModelUsage",
                       "Session.Get", "Session.Pause", "Session.Get", "Session.ModelUsage",
                       "Session.Get", "Session.Resume", "Session.Get", "Session.ModelUsage")
            relaying = pool.submit(self.relay, listener, methods, drop="Session.Pause")
            shell = Shell(node, identity, path, manage_agent_sessions=True)
            self.addCleanup(shell.close)
            args = dict(origin=shell.origin, code=shell.code, agentControl=True, session=session,
                        profile=str(node.base / "browser"))
            subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                            os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                           input=json.dumps(args), text=True, check=True, timeout=35)
            relaying.result(5)
        state = node.call("Session.Get", {"id": session}, actor["token"])
        self.assertEqual(state["session"]["lifecycle"], "AwaitingModel")
        self.assertNotIn("operator_pause", state["session"])

    def test_agent_runtime_paginates_owned_sessions_and_projects_model_usage(self):
        identity, read, actor, visible, hidden, provider = self.agent_sessions(9)
        session = visible[-1]
        lease = self.finish_agent_session(provider, session)
        shell = self.shell(identity)
        self.error(shell, 401, "AuthenticationRequired", "/api/agent-sessions", data={})
        token = shell.pair()["token"]
        self.error(shell, 400, "InvalidArgument", "/api/agent-sessions", data={"kind": "Task"}, token=token)
        status, headers, page = shell.http("/api/agent-sessions", {}, token)
        self.assertEqual(status, 200)
        self.assertEqual(headers["Cache-Control"], "no-store")
        self.assertEqual([row["reference"]["resource_id"] for row in page["items"]], visible[:8])
        self.assertNotIn("private runtime goal", json.dumps(page))
        status, _, page = shell.http("/api/agent-sessions", {"cursor": page["next_cursor"]}, token)
        self.assertEqual(status, 200)
        self.assertIsNone(page["next_cursor"])
        self.assertEqual(len(page["items"]), 1)
        status, _, detail = shell.http("/api/agent-session", {"id": session}, token)
        self.assertEqual(status, 200)
        self.assertEqual(detail["status"], page["items"][0])
        self.assertEqual(detail["status"]["session"]["lifecycle"], "Succeeded")
        self.assertEqual((detail["charged_calls"], detail["provider_reported"]),
                         (1, {"input_tokens": 12, "output_tokens": 3, "reported_calls": 1}))
        core_usage = self.node.call("Session.ModelUsage", {"id": session}, actor["token"])
        self.assertEqual(detail["lease_time"], core_usage["lease_time"])
        self.assertEqual(set(detail), {"status", "charged_calls", "provider_reported", "lease_time"})
        for secret in ("private runtime goal", "private model summary", lease, hidden):
            self.assertNotIn(secret, json.dumps(detail))
        self.error(shell, 403, "PermissionDenied", "/api/agent-session", data={"id": hidden}, token=token)
        self.node.call("Grant.Revoke", target(read))
        self.error(shell, 403, "PermissionDenied", "/api/agent-sessions", data={}, token=token)
        self.error(shell, 403, "PermissionDenied", "/api/agent-session", data={"id": session}, token=token)

    def test_agent_runtime_rejects_model_usage_at_a_different_revision(self):
        identity, _, actor, sessions, _, _ = self.agent_sessions(1)
        node = self.node
        session = sessions[0]
        listening = node.base / "agent-usage.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(listening))
            listener.settimeout(8)
            listener.listen()
            relay = pool.submit(self.relay, listener, ("Principal.WhoAmI", "Session.Get", "Session.ModelUsage"),
                                "Session.Get", waiting, release)
            shell = Shell(node, identity, listening)
            self.addCleanup(shell.close)
            token = shell.pair()["token"]
            detail = pool.submit(shell.http, "/api/agent-session", {"id": session}, token)
            try:
                self.assertTrue(waiting.wait(8))
                previous = node.call("Session.Get", {"id": session}, actor["token"])["reference"]
                paused = node.call("Session.Pause", {**target(previous), "reason": "Operator checkpoint"})
                self.assertEqual(paused["session"]["lifecycle"], "Paused")
            finally:
                release.set()
            relay.result(8)
            status, _, result = detail.result(8)
            self.assertEqual((status, result["error"]["code"]), (409, "Conflict"))

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_agent_runtime_reads_usage_and_clears_revoked_sessions(self):
        identity, read, _, visible, hidden, provider = self.agent_sessions(9)
        self.finish_agent_session(provider, visible[-1])
        shell = self.shell(identity)
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(35)
            args = dict(origin=shell.origin, code=shell.code, agentRuntime=True, session=visible[-1], hidden=hidden,
                        profile=str(self.node.base / "browser"), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"V")
                    self.node.call("Grant.Revoke", target(read))
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=35), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    def test_project_queries_require_session_and_project_grants(self):
        shell = self.shell()
        self.error(shell, 401, "AuthenticationRequired", "/api/projects", data={})
        token = shell.pair()["token"]
        project = self.node.call("Project.Create", {"key": key(), "title": "Private project"})["reference"]
        query = {"project_id": project["resource_id"]}
        self.error(shell, 403, "PermissionDenied", "/api/projects", data={}, token=token)
        self.error(shell, 403, "PermissionDenied", "/api/project-work-items", data=query, token=token)
        self.error(shell, 400, "InvalidArgument", "/api/projects", data={"limit": 32}, token=token)
        self.error(shell, 404, "NotFound", "/api/project-create", data={}, token=token)
        self.node.call("Grant.Issue", {"principal_id": "local-owner", "actions": ["project.read"],
                                     "tool_targets": [query["project_id"]], "units": 1, "ttl_seconds": 120})
        status, _, result = shell.http("/api/project-work-items", query, token)
        self.assertEqual((status, result), (200, {"title": "Private project", "items": [], "next_after_id": None}))

    def work_item_evidence(self):
        node = self.node
        project = node.call("Project.Create", {"key": key(), "title": "交付验收"})["reference"]
        grant = node.call("Grant.Issue", {"principal_id": "local-owner",
                                          "actions": ["project.read", "project.manage", "project.contribute"],
                                          "tool_targets": [project["resource_id"]], "units": 4, "ttl_seconds": 180})
        ready = node.call("WorkItem.Create", {"project_id": project["resource_id"],
            "expected_project_version": project["version"], "key": key(), "title": "核对本机盘点",
            "description": "交付说明 <em>原样保留</em>", "assignee_principal_id": "local-owner",
            "criterion": {"kind": "TaskResult", "tool": "system.status",
                          "target_ref": node.get("profile-local")["reference"]}})["reference"]
        task = node.submit("system.status", {})
        result = node.wait(task)
        self.assertEqual(result["lifecycle"], "Succeeded")
        artifact, = result["result"]["artifact_refs"]
        task_ref = node.get(task)["reference"]
        submitted = node.call("WorkItem.SubmitEvidence", {**target(ready), "task_ref": task_ref, "artifact_ref": artifact})
        self.assertEqual(submitted["value"]["lifecycle"], "InReview")
        return project, grant["grant_ref"], ready, submitted["reference"], task_ref, artifact

    def test_work_item_revision_is_exact_and_uses_current_evidence_authority(self):
        project, _, ready, submitted, task, artifact = self.work_item_evidence()
        shell = self.shell()
        path = "/api/work-item-revision"
        self.error(shell, 401, "AuthenticationRequired", path, data={"reference": submitted})
        token = shell.pair()["token"]
        status, _, revision = shell.http(path, {"reference": ready}, token)
        self.assertEqual((status, revision["reference"], revision["lifecycle"], revision["evidence"]),
                         (200, ready, "Ready", None))
        self.error(shell, 409, "PreconditionFailed", path, token=token,
                   data={"reference": dict(submitted, digest="sha256:" + "0" * 64)})
        status, _, revision = shell.http(path, {"reference": submitted}, token)
        self.assertEqual((status, revision["reference"], revision["lifecycle"]), (200, submitted, "InReview"))
        self.assertEqual(set(revision["evidence"]), {"task_ref", "artifact_ref", "submitted_at"})
        self.assertEqual(revision["evidence"]["task_ref"], task)
        self.assertEqual(revision["evidence"]["artifact_ref"], artifact)

        viewer = self.node.call("Agent.Register", {"name": "work-item-evidence-viewer"})
        self.node.call("Project.SetMember", {**target(project), "principal_id": viewer["principal_id"], "role": "Viewer"})
        self.node.call("Grant.Issue", {"principal_id": viewer["principal_id"], "actions": ["project.read"],
                                      "tool_targets": [project["resource_id"]], "units": 1, "ttl_seconds": 120})
        identity = self.node.base / "project-evidence-viewer.token"
        identity.touch(mode=0o600)
        identity.write_text(viewer["token"])
        viewer_shell = self.shell(identity)
        viewer_token = viewer_shell.pair()["token"]
        status, _, redacted = viewer_shell.http(path, {"reference": submitted}, viewer_token)
        self.assertEqual((status, redacted["lifecycle"], redacted["has_evidence"], redacted["evidence"]),
                         (200, "InReview", True, None))
        serialized = json.dumps(redacted)
        self.assertNotIn(task["resource_id"], serialized)
        self.assertNotIn(artifact["resource_id"], serialized)
        self.error(viewer_shell, 404, "NotFound", "/api/work-item-evidence", token=viewer_token,
                   data={"reference": submitted})

    def test_work_item_evidence_download_uses_the_exact_revision(self):
        _, grant, ready, submitted, task, artifact = self.work_item_evidence()
        shell = self.shell()
        path = "/api/work-item-evidence"
        request = {"reference": submitted}
        self.error(shell, 401, "AuthenticationRequired", path, data=request)
        token = shell.pair()["token"]
        self.error(shell, 404, "NotFound", path, token=token, data={"reference": ready})
        self.error(shell, 409, "PreconditionFailed", path, token=token,
                   data={"reference": dict(submitted, digest="sha256:" + "0" * 64)})
        self.error(shell, 400, "InvalidArgument", path, token=token, data=dict(request, artifact_ref=artifact))
        status, headers, raw = shell.http(path, request, token)
        self.assertEqual((status, headers["Content-Type"], headers["Cache-Control"]),
                         (200, "application/octet-stream", "no-store"))
        self.assertIn("attachment", headers["Content-Disposition"])
        self.assertEqual("sha256:" + hashlib.sha256(raw).hexdigest(), artifact["digest"])
        self.node.call("Grant.Revoke", target(grant))
        self.error(shell, 403, "PermissionDenied", path, token=token, data=request)
        status, _, direct = shell.http("/api/task-artifact", {"task_id": task["resource_id"],
                                       "reference": artifact, "download": True}, token)
        self.assertEqual((status, direct), (200, raw))

    def test_work_item_evidence_download_rechecks_project_after_reading(self):
        node = self.node
        _, grant, _, submitted, _, _ = self.work_item_evidence()
        proxy = node.base / "project-evidence.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)
            forwarded = pool.submit(self.relay, listener,
                                    ("Principal.WhoAmI", "WorkItem.RevisionGet", "Artifact.ReadChunk", "WorkItem.RevisionGet"),
                                    "Artifact.ReadChunk", waiting, release)
            shell = Shell(node, node.witness / "owner.token", proxy)
            self.addCleanup(shell.close)
            token = shell.pair()["token"]
            download = pool.submit(shell.http, "/api/work-item-evidence", {"reference": submitted}, token)
            try:
                self.assertTrue(waiting.wait(8))
                node.call("Grant.Revoke", target(grant))
            finally:
                release.set()
            forwarded.result(8)
            status, headers, fault = download.result(8)
            self.assertEqual((status, fault["error"]["code"]), (403, "PermissionDenied"))
            self.assertIsNone(headers["Content-Disposition"])
            self.assertEqual(headers["Cache-Control"], "no-store")

    def work_item_reviewer(self, project, task):
        node = self.node
        reviewer = node.call("Agent.Register", {"name": "shell-work-item-reviewer"})
        principal = reviewer["principal_id"]
        node.call("Project.SetMember", {**target(project), "principal_id": principal, "role": "Reviewer"})
        node.call("Grant.Issue", {"principal_id": principal, "actions": ["project.read", "project.review"],
                                 "tool_targets": [project["resource_id"]], "units": 2, "ttl_seconds": 160})
        result = node.call("Grant.Issue", {"principal_id": principal, "actions": ["task.results.read"],
                                         "tool_targets": [task["resource_id"]], "units": 1, "ttl_seconds": 160})
        identity = node.base / "reviewer.token"
        identity.touch(mode=0o600)
        identity.write_text(reviewer["token"])
        return reviewer, identity, result["grant_ref"]

    def test_work_item_review_is_opt_in_and_binds_the_exact_pending_revision(self):
        project, _, _, submitted, task, _ = self.work_item_evidence()
        reviewer, identity, _ = self.work_item_reviewer(project, task)
        path = "/api/review-work-item"
        request = {"reference": submitted, "accept": False}
        default = self.shell(identity)
        paired = default.pair()
        self.assertFalse(paired["review_work_items"])
        self.error(default, 404, "NotFound", path, token=paired["token"], data=request)
        owner = self.shell(review_work_items=True)
        self.error(owner, 403, "PermissionDenied", path, token=owner.pair()["token"], data=request)
        shell = self.shell(identity, review_work_items=True)
        self.error(shell, 401, "AuthenticationRequired", path, data=request)
        paired = shell.pair()
        self.assertTrue(paired["review_work_items"])
        token = paired["token"]
        self.error(shell, 400, "InvalidArgument", path, token=token, data=dict(request, project_id=project["resource_id"]))
        self.error(shell, 409, "PreconditionFailed", path, token=token,
                   data=dict(request, reference=dict(submitted, digest="sha256:" + "0" * 64)))
        status, _, receipt = shell.http(path, request, token)
        current = self.node.call("WorkItem.Get", {"id": submitted["resource_id"]})
        self.assertEqual((status, receipt), (200, {"reference": current["reference"]}))
        self.assertEqual(current["value"]["lifecycle"], "Ready")
        self.assertEqual((current["value"]["review"]["reviewer_principal_id"], current["value"]["review"]["accepted"]),
                         (reviewer["principal_id"], False))
        self.error(shell, 409, "Conflict", path, token=token, data=dict(request, accept=True))
        self.assertEqual(self.node.call("WorkItem.Get", {"id": submitted["resource_id"]}), current)

    def test_work_item_review_rechecks_result_grant_before_commit(self):
        project, _, _, submitted, task, _ = self.work_item_evidence()
        _, identity, grant = self.work_item_reviewer(project, task)
        proxy = self.node.base / "review-race.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)
            methods = ("Principal.WhoAmI", "WorkItem.RevisionGet", "WorkItem.Review")
            forwarded = pool.submit(self.relay, listener, methods, "WorkItem.RevisionGet", waiting, release)
            shell = Shell(self.node, identity, proxy, review_work_items=True)
            self.addCleanup(shell.close)
            request = {"reference": submitted, "accept": True}
            review = pool.submit(shell.http, "/api/review-work-item", request, shell.pair()["token"])
            try:
                self.assertTrue(waiting.wait(8))
                self.node.call("Grant.Revoke", target(grant))
            finally:
                release.set()
            forwarded.result(8)
            status, _, fault = review.result(8)
            self.assertEqual((status, fault["error"]["code"]), (403, "PermissionDenied"))
            self.assertEqual(self.node.call("WorkItem.Get", {"id": submitted["resource_id"]})["reference"], submitted)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_can_recover_a_lost_work_item_review_reply(self):
        node = self.node
        project, _, _, submitted, task, _ = self.work_item_evidence()
        reviewer, identity, _ = self.work_item_reviewer(project, task)
        proxy = node.base / "review-response.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(1) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(34)
            methods = ("Principal.WhoAmI", "Project.List", "WorkItem.List", "Project.Get", "WorkItem.RevisionGet",
                       "WorkItem.RevisionGet", "WorkItem.Review", "WorkItem.List", "Project.Get", "WorkItem.RevisionGet")
            forwarded = pool.submit(self.relay, listener, methods, drop="WorkItem.Review")
            shell = Shell(node, identity, proxy, review_work_items=True)
            self.addCleanup(shell.close)
            args = dict(origin=shell.origin, code=shell.code, projectReview=True, project=project["resource_id"],
                        work_item=submitted, reviewer=reviewer["principal_id"], profile=str(node.base / "browser"))
            subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                            os.environ.get("PSIOS_TEST_CHROME", "google-chrome")], input=json.dumps(args).encode(),
                           check=True, timeout=35)
            forwarded.result(8)
        current = node.call("WorkItem.Get", {"id": submitted["resource_id"]})
        self.assertEqual(current["value"]["lifecycle"], "Verified")
        self.assertEqual(current["value"]["review"]["reviewer_principal_id"], reviewer["principal_id"])
        history = node.call("WorkItem.History", {"id": submitted["resource_id"]})["items"]
        self.assertEqual([record["reference"] for record in history[-2:]], [submitted, current["reference"]])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_reads_fixed_work_item_evidence_and_clears_revoked_project(self):
        project, grant, _, submitted, task, artifact = self.work_item_evidence()
        shell = self.shell()
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(35)
            downloads = self.node.base / "downloads"
            downloads.mkdir()
            args = dict(origin=shell.origin, code=shell.code, projectEvidence=True, project=project["resource_id"],
                        work_item=submitted, evidence_task=task, evidence_artifact=artifact,
                        profile=str(self.node.base / "browser"), downloads=str(downloads), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    cancelled = self.node.call("WorkItem.Abandon", {**target(submitted), "reason": "Replacement accepted"})
                    self.assertEqual(cancelled["value"]["lifecycle"], "Abandoned")
                    parent.sendall(b"K")
                    self.assertEqual(parent.recv(1), b"V")
                    self.node.call("Grant.Revoke", target(grant))
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=35), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_reads_project_work_items_and_rechecks_membership(self):
        node = self.node
        viewer = node.call("Agent.Register", {"name": "shell-project-viewer"})
        visible = sorted((node.call("Project.Create", {"key": key(), "title": f"实施项目 {n}"})["reference"]
                          for n in range(9)), key=lambda ref: ref["resource_id"])
        secret = node.call("Project.Create", {"key": key(), "title": "无成员资格的项目"})["reference"]
        ids = [ref["resource_id"] for ref in [*visible, secret]]
        node.call("Grant.Issue", {"principal_id": "local-owner", "actions": ["project.read", "project.manage"],
                                 "tool_targets": ids, "units": 8, "ttl_seconds": 240})
        node.call("Grant.Issue", {"principal_id": viewer["principal_id"], "actions": ["project.read"],
                                 "tool_targets": ids, "units": 1, "ttl_seconds": 240})
        visible = [node.call("Project.SetMember", {**target(ref), "principal_id": viewer["principal_id"],
                                                    "role": "Viewer"})["reference"] for ref in visible]
        active = visible[-1]
        profile = node.get("profile-local")["reference"]

        def item(project, title, **fields):
            return node.call("WorkItem.Create", {"key": key(), "title": title, "description": "仅限原始协议详情",
                "project_id": project["resource_id"], "expected_project_version": project["version"],
                "assignee_principal_id": "local-owner", "criterion": {"kind": "TaskResult", "tool": "system.status",
                                                                       "target_ref": profile}, **fields})["reference"]

        predecessor = item(active, "核对准备条件")
        blocked = item(active, "<b>核对交付条件</b>", priority="High", blocked_by=[predecessor["resource_id"]])
        abandoned = item(active, "已停止的工作")
        node.call("WorkItem.Abandon", {**target(abandoned), "reason": "不应出现在项目摘要中的终止原因"})
        for n in range(10):
            item(active, f"核对记录 {n}")
        unrelated = item(visible[0], "另一个项目的工作")

        identity = node.base / "project-viewer.token"
        identity.touch(mode=0o600)
        identity.write_text(viewer["token"])
        inventory = self.shell(identity)
        token = inventory.pair()["token"]
        self.error(inventory, 403, "PermissionDenied", "/api/project-work-items", token=token,
                   data={"project_id": secret["resource_id"]})
        self.error(inventory, 400, "InvalidArgument", "/api/project-work-items", token=token,
                   data={"project_id": active["resource_id"], "after_id": unrelated["resource_id"]})
        status, _, first = inventory.http("/api/project-work-items", {"project_id": active["resource_id"]}, token)
        self.assertEqual((status, len(first["items"])), (200, 12))
        status, _, last = inventory.http("/api/project-work-items",
                                        {"project_id": active["resource_id"], "after_id": first["next_after_id"]}, token)
        self.assertEqual((status, len(last["items"]), last["next_after_id"]), (200, 1, None))
        result = {entry["id"]: entry for entry in first["items"] + last["items"]}
        self.assertEqual(result[blocked["resource_id"]], {"id": blocked["resource_id"], "reference": blocked,
                         "title": "<b>核对交付条件</b>",
                         "lifecycle": "Ready", "assignee": "local-owner", "priority": "High", "blocked": True})
        self.assertEqual(result[abandoned["resource_id"]]["lifecycle"], "Abandoned")
        self.assertNotIn("仅限原始协议详情", json.dumps(result, ensure_ascii=False))
        self.assertNotIn("不应出现在项目摘要中的终止原因", json.dumps(result, ensure_ascii=False))

        shell = self.shell(identity)
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(45)
            args = dict(origin=shell.origin, code=shell.code, projects=True, project=active["resource_id"],
                        work_item=blocked["resource_id"], profile=str(node.base / "browser"), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    node.call("Project.SetMember", {**target(active), "principal_id": viewer["principal_id"], "role": None})
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=45), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()
        self.error(inventory, 403, "PermissionDenied", "/api/project-work-items",
                   token=token, data={"project_id": active["resource_id"]})

    def test_chat_directory_requires_the_paired_principal_scope(self):
        shell = self.shell()
        self.error(shell, 401, "AuthenticationRequired", "/api/conversations", data={})
        token = shell.pair()["token"]
        self.error(shell, 403, "PermissionDenied", "/api/conversations", data={}, token=token)
        self.error(shell, 400, "InvalidArgument", "/api/conversation-messages",
                   data={"id": "room-test", "limit": 32}, token=token)
        self.error(shell, 404, "NotFound", "/api/chat-prepare", data={}, token=token)
        self.error(shell, 404, "NotFound", "/api/chat-send", data={}, token=token)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_reads_agent_chat_and_removes_an_ended_membership(self):
        node = self.node
        owner, _ = node.agent(["chat.invite", "chat.read", "chat.send"])
        viewer, _ = node.agent(["chat.read", "resource.read", "task.read", "task.submit", "system.status"])
        room = node.call("Conversation.OpenRoom",
                         {"key": key(), "name": "现场排查", "capacity": 3, "ttl_seconds": 240}, owner["token"])
        def version(chat):
            return {"id": chat["id"], "expected_resource_version": chat["resource_version"]}

        namespace = node.call("Message.OpenSendWindow",
                              {"id": room["id"], "ttl_seconds": 180}, owner["token"])["namespace"]

        def send(body):
            prepared = node.call("Message.Prepare", {"conversation_id": room["id"],
                                 "client_send_id": namespace + ":" + key(), "text": body}, owner["token"])
            node.call("Message.Send", prepared, owner["token"])

        send("before membership, private")
        invitation = node.call("Conversation.Invite", dict(version(room), principal_id=viewer["principal_id"],
                                                           ttl_seconds=120), owner["token"])
        member = invitation["membership_id"]
        joined = node.call("Conversation.Respond", dict(version(invitation["conversation"]), membership_id=member,
                                                       accept=True), viewer["token"])
        for number in range(1, 19):
            send(f"现场记录 {number}\n<em>以文字显示</em>")
        for number in range(12):
            node.call("Conversation.OpenDirect", {"key": key(), "name": f"对话 {number}",
                      "recipient_principal_id": viewer["principal_id"], "ttl_seconds": 120}, owner["token"])

        current = node.call("Message.List", {"id": room["id"]}, viewer["token"])
        baseline_read = current["read_sequence"]
        self.assertEqual(len(current["items"]), 16)
        self.assertEqual(current["items"][0]["content"]["parts"][0]["text"].splitlines()[0], "现场记录 1")

        identity = node.base / "chat-viewer.token"
        identity.touch(mode=0o600)
        identity.write_text(viewer["token"])
        shell = self.shell(identity)
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(45)
            args = dict(origin=shell.origin, code=shell.code, chat=True, conversation=room["id"],
                        profile=str(node.base / "browser"), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    self.assertEqual(node.call("Message.List", {"id": room["id"]}, viewer["token"])["read_sequence"],
                                     baseline_read)
                    current_room = node.call("Conversation.Get", {"id": joined["id"]}, owner["token"])
                    node.call("Conversation.RemoveMember", dict(version(current_room), membership_id=member),
                              owner["token"])
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=45), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    def chat_sender(self):
        actor, grant = self.node.agent(["chat.invite", "chat.read", "chat.send"])
        room = self.node.call("Conversation.OpenRoom",
                              {"key": key(), "name": "现场交接", "capacity": 2, "ttl_seconds": 240},
                              actor["token"])
        token_file = self.node.base / "chat-writer.token"
        token_file.touch(mode=0o600)
        token_file.write_text(actor["token"])
        return actor, grant, room["id"], token_file

    def test_chat_send_requires_explicit_enable_and_rechecks_the_core_grant(self):
        actor, grant, room, identity = self.chat_sender()
        shell = self.shell(identity, send_chat=True)
        session = shell.pair()
        self.assertTrue(session["send_chat"])
        token = session["token"]
        text = '"' * 4096
        self.error(shell, 400, "InvalidArgument", "/api/chat-prepare",
                   data={"id": room, "text": text + "x"}, token=token)
        self.error(shell, 413, "InvalidArgument", "/api/chat-prepare",
                   data={"id": room, "text": "x" * 13000}, token=token)
        code, _, draft = shell.http("/api/chat-prepare", {"id": room, "text": text}, token)
        self.assertEqual(code, 200)
        self.assertEqual(draft["prepared"]["content"]["parts"][0]["text"], text)
        accepted_status, _, accepted = shell.http("/api/chat-send", draft, token)
        self.assertEqual(accepted_status, 200)
        self.assertFalse(accepted["deduplicated"])
        response, _, replayed = shell.http("/api/chat-send", draft, token)
        self.assertEqual(response, 200)
        self.assertEqual(replayed, dict(accepted, deduplicated=True))
        items = self.node.call("Message.List", {"id": room}, actor["token"])["items"]
        self.assertEqual([item["content"]["parts"][0]["text"] for item in items], [text])

        code, _, another = shell.http("/api/chat-prepare", {"id": room, "text": "still pending"}, token)
        self.assertEqual(code, 200)
        self.node.call("Grant.Revoke", target(grant["grant_ref"]))
        self.error(shell, 403, "PermissionDenied", "/api/chat-send", data=another, token=token)
        self.node.call("Grant.Issue", {"principal_id": actor["principal_id"], "actions": ["chat.read"],
                                      "units": 1, "ttl_seconds": 120})
        items = self.node.call("Message.List", {"id": room}, actor["token"])["items"]
        self.assertEqual(len(items), 1)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_chat_retries_only_the_same_send_after_a_lost_receipt(self):
        actor, _, room, identity = self.chat_sender()
        proxy = self.node.base / "chat-reply.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(max_workers=1) as pool:
            listener.bind(str(proxy))
            listener.settimeout(16)
            listener.listen()
            methods = ["Principal.WhoAmI", "Conversation.List", "Message.List",
                       "Message.OpenSendWindow", "Message.Prepare", "Message.Send", "Message.Send", "Message.List"]
            relaying = pool.submit(self.relay, listener, methods, drop="Message.Send")
            shell = Shell(self.node, identity, proxy, send_chat=True)
            self.addCleanup(shell.close)
            parent, child = socket.socketpair()
            with parent, child:
                parent.settimeout(30)
                args = dict(origin=shell.origin, code=shell.code, chatSend=True, conversation=room,
                            profile=str(self.node.base / "browser"), control_fd=child.fileno())
                with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                       os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                      stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                    child.close()
                    try:
                        browser.stdin.write(json.dumps(args).encode())
                        browser.stdin.close()
                        self.assertEqual(parent.recv(1), b"R")
                        committed = self.node.call("Message.List", {"id": room}, actor["token"])["items"]
                        self.assertEqual(len(committed), 1)
                        parent.sendall(b"K")
                        self.assertEqual(browser.wait(timeout=35), 0)
                    finally:
                        if browser.poll() is None:
                            browser.kill()
            relaying.result(timeout=5)
        self.assertEqual(self.node.call("Message.List", {"id": room}, actor["token"])["items"], committed)

    def test_application_catalog_only_exposes_its_fixed_queries(self):
        shell = self.shell()
        self.error(shell, 401, "AuthenticationRequired", "/api/applications", data={})
        bearer = shell.pair()["token"]
        status, _, page = shell.http("/api/applications", {}, bearer)
        self.assertEqual((status, page), (200, {"items": [], "next_cursor": None}))
        self.error(shell, 400, "InvalidArgument", "/api/applications",
                   data={"kind": "Secret"}, token=bearer)
        self.error(shell, 400, "InvalidArgument", "/api/application-releases",
                   data={"id": "app-missing", "limit": 64}, token=bearer)
        self.error(shell, 403, "PermissionDenied", "/api/application-releases",
                   data={"id": "app-missing"}, token=bearer)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_OCI"), "requires real local Docker and Chromium")
    def test_browser_pages_the_managed_application_catalog_and_fences_revoked_releases(self):
        node = self.node
        docker = ["docker", "--host", "unix:///var/run/docker.sock"]
        tag = "psios-shell-check:catalog-" + node.base.name
        source = node.base / "probe.c"
        program = node.base / "probe"
        source.write_text("int main(void) { return 0; }\n")
        subprocess.run(["cc", "-static", "-o", program, source], check=True, timeout=30)
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as data:
            data.add(program, arcname="probe")
        image_id = subprocess.check_output([*docker, "import", "-", tag],
                                           input=archive.getvalue(), timeout=45).decode().strip()
        self.addCleanup(subprocess.run, [*docker, "image", "rm", tag], check=True,
                        stdout=subprocess.DEVNULL, timeout=15)
        spec = dict(image_id=image_id, name="catalog-0", release_version="1.0.0", argv=["/probe"],
                    license_expression="0BSD", cpu_millicores=200, memory_max_bytes=33554432,
                    pids_max=8, ephemeral_max_bytes=1048576)
        applications = [node.perform("application.install", dict(spec, name=f"catalog-{i}")) for i in range(9)]
        app = applications[0]
        aid = app["resource_id"]
        for revision in range(2, 10):
            current = node.get(aid)["reference"]
            node.perform("application.stage_release", {
                "target": target(current), "release": dict(spec, release_version=f"{revision}.0.0"),
            })
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "tool_targets": [aid],
                                         "actions": ["application.releases"], "ttl_seconds": 240, "units": 10})
        inventory = self.shell()
        token = inventory.pair()["token"]
        status, _, page = inventory.http("/api/applications", {}, token)
        self.assertEqual(status, 200)
        self.assertEqual(len(page["items"]), 8)
        self.assertNotIn(image_id, json.dumps(page))
        for item in page["items"]:
            self.assertEqual(set(item), {"reference", "name", "lifecycle"})
        status, _, final_page = inventory.http("/api/applications", {"cursor": page["next_cursor"]}, token)
        self.assertEqual(status, 200)
        self.assertEqual([item["reference"]["resource_id"] for item in final_page["items"]], [applications[-1]["resource_id"]])
        self.error(inventory, 409, "CursorExpired", "/api/application-releases",
                   data={"id": aid, "cursor": page["next_cursor"]}, token=token)
        self.assertEqual(inventory.http("/api/logout", {}, token)[0], 200)

        other, _ = node.agent(["resource.read", "task.read"])
        token_file = node.base / "catalog-reader.token"
        token_file.touch(mode=0o600)
        token_file.write_text(other["token"])
        unrelated = self.shell(token_file)
        token = unrelated.pair()["token"]
        self.assertEqual(unrelated.http("/api/applications", {}, token)[2]["items"], [])
        self.error(unrelated, 403, "PermissionDenied", "/api/application-releases",
                   data={"id": aid}, token=token)
        self.assertEqual(unrelated.http("/api/logout", {}, token)[0], 200)

        shell = self.shell()
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(45)
            args = dict(origin=shell.origin, code=shell.code, applicationCatalog=True, application=aid,
                        profile=str(node.base / "browser"), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    node.call("Grant.Revoke", target(grant["grant_ref"]))
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=45), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    def test_one_passive_session_observer_leaves_core_calls_available(self):
        shell = self.shell()
        token = shell.pair()["token"]
        self.error(shell, 401, "AuthenticationRequired", "/api/session-end", data={})
        self.error(shell, 401, "AuthenticationRequired", "/api/session-end", data={}, token="x" * 32)
        self.error(shell, 400, "InvalidArgument", "/api/session-end", data={"token": token}, token=token)
        with ThreadPoolExecutor(max_workers=2) as pool:
            waiting = [pool.submit(shell.http, "/api/session-end", {}, token) for _ in range(2)]
            done, pending = wait(waiting, timeout=5, return_when=FIRST_COMPLETED)
            self.assertEqual(len(done), 1)
            status, _, error = done.pop().result()
            self.assertEqual((status, error["error"]["code"]), (409, "Conflict"))
            self.assertEqual(shell.http("/api/node", {}, token)[0], 200)
            self.assertEqual(shell.http("/api/logout", {}, token)[0], 200)
            status, _, ended = pending.pop().result(timeout=5)
            self.assertEqual((status, ended), (200, {"ended": True}))

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_back_to_a_paired_page_requires_a_new_session(self):
        shell = self.shell()
        args = dict(origin=shell.origin, code=shell.code, navigateBack=True,
                    node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                    profile=str(self.node.base / "browser"))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=50)
        self.error(shell, 401, "AuthenticationRequired", "/api/session", data={"code": shell.code})

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_application_launcher_filters_opens_and_restores_windows(self):
        shell = self.shell()
        args = dict(origin=shell.origin, code=shell.code, applicationLauncher=True,
                    node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                    profile=str(self.node.base / "browser"))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=60)
        self.assertEqual(shell.process.wait(timeout=5), 0)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_tiles_windows_with_keyboard_and_restores_them_across_viewport_changes(self):
        task = self.node.submit("system.status", {})
        before = self.node.wait(task)
        self.assertEqual(before["lifecycle"], "Succeeded")
        shell = self.shell()
        args = dict(origin=shell.origin, code=shell.code, windowTiling=True, task=task,
                    node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                    profile=str(self.node.base / "browser"))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=60)
        self.assertEqual(self.node.call("Task.Get", {"id": task}), before)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_manages_named_workspaces_without_changing_tasks(self):
        task = self.node.submit("system.status", {})
        before = self.node.wait(task)
        self.assertEqual(before["lifecycle"], "Succeeded")
        shell = self.shell()
        args = dict(origin=shell.origin, code=shell.code, namedWorkspaces=True,
                    node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                    profile=str(self.node.base / "browser"))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=60)
        self.assertEqual(shell.process.wait(timeout=5), 0)
        self.assertEqual(self.node.call("Task.Get", {"id": task}), before)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_exports_and_restores_only_the_current_identity_layout(self):
        task = self.node.submit("system.status", {})
        before = self.node.wait(task)
        self.assertEqual(before["lifecycle"], "Succeeded")
        other = self.node.call("Agent.Register", {"name": "layout-other-principal"})
        identity = self.node.base / "layout-other.token"
        identity.touch(mode=0o600)
        identity.write_text(other["token"])
        _, _, files = self.file_volume()
        writer, reader, refused = self.shell(files=files), self.shell(), self.shell(identity)
        another = Node(self.node.binaries)
        self.addCleanup(another.close)
        other_node = Shell(another, another.witness / "owner.token")
        self.addCleanup(other_node.close)
        downloads = self.node.base / "layouts"
        downloads.mkdir(mode=0o700)
        args = dict(origin=writer.origin, code=writer.code, layoutTransfer=True, task=task,
                    reader=dict(origin=reader.origin, code=reader.code),
                    other=dict(origin=refused.origin, code=refused.code),
                    otherNode=dict(origin=other_node.origin, code=other_node.code), volume=files[0],
                    node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                    profile=str(self.node.base / "browser"), downloads=str(downloads))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=75)
        for shell in (writer, reader, refused, other_node):
            self.assertEqual(shell.process.wait(timeout=5), 0)
        self.assertEqual(self.node.call("Task.Get", {"id": task}), before)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_moves_and_resizes_windows_without_changing_tasks(self):
        task = self.node.submit("system.status", {})
        before = self.node.wait(task)
        self.assertEqual(before["lifecycle"], "Succeeded")
        shell = self.shell()
        args = dict(origin=shell.origin, code=shell.code, windowGeometry=True,
                    node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                    profile=str(self.node.base / "browser"))
        subprocess.run(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                        os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                       input=json.dumps(args), text=True, check=True, timeout=60)
        self.assertEqual(shell.process.wait(timeout=5), 0)
        self.assertEqual(self.node.call("Task.Get", {"id": task}), before)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_pairs_reads_tasks_and_restores_selection_focus(self):
        task = self.node.submit("system.status", {})
        result = self.node.wait(task)
        self.assertEqual(result["lifecycle"], "Succeeded")
        digest = result["result"]["artifact_refs"][0]["digest"]
        downloads = self.node.base / "downloads"
        downloads.mkdir(mode=0o700)
        large_task, large_ref, _ = self.large_result()
        for _ in range(17):
            queued = self.node.submit("system.status", {})
            self.assertEqual(self.node.wait(queued)["lifecycle"], "Succeeded")
        node = self.node.call("Node.Get", {})
        self.node.call("Node.Configure", {"expected_resource_version": node["reference"]["version"], "scheduling": "Cordoned"})
        pending = self.node.submit("system.status", {})
        # Exercise independent admission floors on the same live filesystem. Reads
        # and cancellation of the already queued Task remain available under pressure.
        self.node.stop()
        cfg = json.loads(self.node.config.read_text())
        floor = 2**53 - 1
        cfg["authority_reserve"] = {"state_inodes": floor, "witness_bytes": floor}
        self.node.config.write_text(json.dumps(cfg))
        self.node.start()
        storage = self.node.call("Node.Get", {})["authority_storage"]
        self.assertTrue(storage["witness"]["below_reserve"])
        inode_observed = storage["state"]["inode_accounting"] == "Observed"
        self.assertEqual(storage["state"]["below_reserve"], inode_observed)
        shell = self.shell(cancel_pending=True)
        browser = os.environ.get("PSIOS_TEST_CHROME", "google-chrome")
        subprocess.run(
            ["node", str(Path(__file__).with_name("shell_browser.mjs")), browser],
            input=json.dumps(dict(origin=shell.origin, code=shell.code, task=task, digest=digest,
                                  history=self.node.call("Task.History", {"id": task})["items"], pending=pending,
                                  large_task=large_task, large_digest=large_ref["digest"], downloads=str(downloads),
                                  node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                                  inode_observed=inode_observed, profile=str(self.node.base / "browser"))),
            text=True, timeout=110, check=True,
        )
        self.assertEqual(shell.process.wait(timeout=5), 0)
        self.assertEqual(self.node.call("Task.Get", {"id": pending})["lifecycle"], "Cancelled")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_restoring_an_app_rechecks_its_grant_without_losing_the_other_window(self):
        self.check_window_restoration(transition="minimize")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_revealing_a_covered_app_rechecks_its_grant_without_losing_the_maximized_task(self):
        self.check_window_restoration(transition="cover")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_returning_to_a_workspace_rechecks_its_grants(self):
        self.check_window_restoration(transition="workspace")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_scrubs_both_live_windows_on_cli_exit(self):
        self.check_ended_session(signal.SIGTERM)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_scrubs_both_live_windows_on_cli_kill(self):
        self.check_ended_session(signal.SIGKILL)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_REAL_IDLE"), "waits for the real five-minute idle deadline")
    def test_browser_idle_session_ends_without_user_input(self):
        self.check_ended_session(None)

    def check_ended_session(self, exit_signal):
        node = self.node
        task = node.submit("system.status", {})
        self.assertEqual(node.wait(task)["lifecycle"], "Succeeded")
        shell = self.shell()
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(45)
            args = dict(origin=shell.origin, code=shell.code, endSession=True, realIdle=exit_signal is None, task=task,
                        node=node.call("Node.Get", {})["reference"]["resource_id"],
                        profile=str(node.base / "browser"), control_fd=child.fileno())
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    started = time.monotonic()
                    if exit_signal:
                        shell.process.send_signal(exit_signal)
                    self.assertEqual(shell.process.wait(timeout=10 if exit_signal else 320),
                                     -signal.SIGKILL if exit_signal == signal.SIGKILL else 0)
                    if not exit_signal:
                        self.assertGreaterEqual(time.monotonic() - started, 285)
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=30), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()

    def check_window_restoration(self, *, transition):
        node = self.node
        agent, work = node.agent(["task.submit", "system.status", "task.read", "resource.read"])
        inventory = node.call("Grant.Issue", {"principal_id": agent["principal_id"],
                                              "actions": ["node.inventory"], "units": 8, "ttl_seconds": 300})
        prepared = node.prepare("system.status", {}, token=agent["token"],
                                delegation=work["delegation_ref"]["resource_id"])
        task = node.call("Task.Submit", prepared, agent["token"])["task_id"]
        self.assertEqual(node.wait(task)["lifecycle"], "Succeeded")
        identity = node.base / "reader.token"
        identity.touch(mode=0o600)
        identity.write_text(agent["token"])
        shell = self.shell(identity)
        parent, child = socket.socketpair()
        with parent, child:
            parent.settimeout(45)
            args = dict(origin=shell.origin, code=shell.code, task=task,
                        node=node.call("Node.Get", {})["reference"]["resource_id"],
                        profile=str(node.base / "browser"), control_fd=child.fileno(), windowTransition=transition)
            with subprocess.Popen(["node", str(Path(__file__).with_name("shell_browser.mjs")),
                                   os.environ.get("PSIOS_TEST_CHROME", "google-chrome")],
                                  stdin=subprocess.PIPE, pass_fds=(child.fileno(),)) as browser:
                child.close()
                try:
                    browser.stdin.write(json.dumps(args).encode())
                    browser.stdin.close()
                    self.assertEqual(parent.recv(1), b"R")
                    node.call("Grant.Revoke", target(inventory["grant_ref"]))
                    parent.sendall(b"K")
                    self.assertEqual(browser.wait(timeout=55), 0)
                finally:
                    if browser.poll() is None:
                        browser.kill()
        self.assertEqual(shell.process.wait(timeout=5), 0)
        self.assertEqual(node.call("Task.Get", {"id": task})["lifecycle"], "Succeeded")

    def test_isolates_credentials_and_only_exposes_bounded_read_routes(self):
        shell = self.shell()
        status, headers, page = shell.http("/", method="GET")
        self.assertEqual(status, 200)
        self.assertIn(b"PsiOS Shell", page)
        self.assertEqual(headers["Cache-Control"], "no-store")
        self.assertEqual(headers["Referrer-Policy"], "no-referrer")
        self.assertIn("frame-ancestors 'none'", headers["Content-Security-Policy"])
        self.assertEqual(headers["Cross-Origin-Resource-Policy"], "same-origin")
        self.assertFalse(shell.code.encode() in page or self.node.owner.strip().encode() in page)
        for asset, kind in [("/shell.js", "text/javascript"), ("/shell.css", "text/css")]:
            reply, headers, asset_body = shell.http(asset, method="GET")
            self.assertEqual(reply, 200)
            self.assertTrue(headers["Content-Type"].startswith(kind))
            self.assertTrue(len(asset_body) > 0)
        self.error(shell, 403, "PermissionDenied", "/", method="GET",
                   headers={"Host": "localhost:" + str(shell.port)})
        self.error(shell, 403, "PermissionDenied", "/?ignored=1", method="GET")
        for headers in [{"Origin": "null"}, {"Origin": "http://localhost"},
                        {"Sec-Fetch-Site": "cross-site"}]:
            self.error(shell, 403, "PermissionDenied", "/api/session",
                       data={"code": shell.code}, headers=headers)
        self.error(shell, 415, "InvalidArgument", "/api/session",
                   data={"code": shell.code}, headers={"Content-Type": "text/plain"})
        self.error(shell, 413, "InvalidArgument", "/api/session", raw=b"x" * 4097)
        self.error(shell, 400, "InvalidArgument", "/api/session",
                   raw=b'{"code":"x","code":"y"}')
        self.error(shell, 401, "AuthenticationRequired", "/api/node", data={})

        with ThreadPoolExecutor(max_workers=2) as workers:
            replies = list(workers.map(lambda _: shell.http("/api/session", {"code": shell.code}), range(2)))
        self.assertEqual(sorted(reply[0] for reply in replies), [200, 401])
        _, headers, session = next(reply for reply in replies if reply[0] == 200)
        self.assertIsNone(headers["Set-Cookie"])
        self.assertEqual(session["principal_id"], self.node.call("Principal.WhoAmI", {})["principal_id"])
        token = session["token"]
        self.assertTrue(token != shell.code and token != self.node.owner.strip())
        self.error(shell, 401, "AuthenticationRequired", "/api/node", data={}, token=shell.code)
        self.error(shell, 404, "NotFound", "/api/call", data={"method": "Task.Submit"}, token=token)
        self.error(shell, 404, "NotFound", "/api/node", method="GET", token=token)
        self.error(shell, 405, "InvalidArgument", "/api/node", method="OPTIONS", token=token)
        self.error(shell, 400, "InvalidArgument", "/api/tasks", data={"kind": "Secret"}, token=token)
        self.error(shell, 409, "CursorExpired", "/api/tasks", data={"cursor": "invalid"}, token=token)
        self.assertEqual(shell.http("/api/logout", {}, token)[0], 200)
        self.assertEqual(shell.process.wait(timeout=5), 0)
        replacement = self.shell()
        self.error(replacement, 401, "AuthenticationRequired", "/api/node", data={}, token=token)

    def test_pending_cancellation_needs_opt_in_current_revision_and_authority(self):
        node = self.node
        volume = node.perform("volume.create", {"name": "shell-cancel"})
        task = node.submit("volume.write", {**target(volume), "name": "pending", "content": "unissued"})
        pending = node.call("Task.Get", {"id": task})
        self.assertEqual(pending["lifecycle"], "WaitingApproval")
        request = {"id": task, "expected_resource_version": pending["task_revision"]}
        route = "/api/cancel-pending-task"
        default = self.shell()
        session = default.pair()
        self.assertFalse(session["cancel_pending_tasks"])
        self.error(default, 404, "NotFound", route, data=request, token=session["token"])

        agent, grant = node.agent(["task.submit", "system.status"])
        read_grant = node.call("Grant.Issue", {"principal_id": agent["principal_id"], "actions": ["task.read"],
                                              "units": 2, "ttl_seconds": 60})
        credential = node.base / "cancel-reader.token"
        credential.touch(mode=0o600)
        credential.write_text(agent["token"])
        scoped = self.shell(credential, cancel_pending=True)
        token = scoped.pair()["token"]
        self.error(scoped, 403, "PermissionDenied", route, data=request, token=token)

        operator = self.shell(cancel_pending=True)
        session = operator.pair()
        self.assertTrue(session["cancel_pending_tasks"])
        owner = session["token"]
        self.error(operator, 400, "InvalidArgument", route, data={"id": task}, token=owner)
        self.error(operator, 409, "Conflict", route, data={**request, "expected_resource_version": "r999"}, token=owner)
        self.error(operator, 403, "PermissionDenied", route, data=request, token=owner,
                   headers={"Origin": "https://example.invalid"})
        self.assertEqual(node.call("Task.Get", {"id": task}), pending)
        status, _, cancelled = operator.http(route, request, owner)
        self.assertEqual((status, cancelled["lifecycle"], cancelled["outcome_knowledge"]), (200, "Cancelled", "NotStarted"))
        self.error(operator, 409, "PreconditionFailed", route,
                   data={**request, "expected_resource_version": cancelled["task_revision"]}, token=owner)
        self.assertFalse((node.state / "volumes" / volume["resource_id"] / "data" / "pending").exists())

        configuration = node.call("Node.Get", {})["reference"]
        node.call("Node.Configure", {"expected_resource_version": configuration["version"], "scheduling": "Cordoned"})
        work = node.prepare("system.status", {}, token=agent["token"], delegation=grant["delegation_ref"]["resource_id"])
        own = node.call("Task.Submit", work, agent["token"])["task_id"]
        fresh = node.call("Task.Get", {"id": own}, agent["token"])
        self.assertEqual(fresh["lifecycle"], "Queued")
        node.call("Grant.Revoke", target(read_grant["grant_ref"]))
        self.error(scoped, 403, "PermissionDenied", route,
                   data={"id": own, "expected_resource_version": fresh["task_revision"]}, token=token)
        self.assertEqual(node.call("Task.Get", {"id": own}), fresh)
        node.stop(signal.SIGKILL)
        node.start()
        self.assertEqual(node.call("Task.Get", {"id": task}), cancelled)

    def test_reads_paged_tasks_and_recovers_after_core_restart(self):
        shell = self.shell()
        token = shell.pair()["token"]
        representative = self.node.submit("system.status", {})
        completed = self.node.wait(representative)
        self.assertEqual(completed["lifecycle"], "Succeeded")
        tasks = {representative}
        for _ in range(21):
            tasks.add(self.node.submit("system.status", {}))
        status, _, page = shell.http("/api/tasks", {}, token)
        self.assertEqual(status, 200)
        self.assertEqual(len(page["items"]), 20)
        self.assertFalse(page["complete"])
        head = {item["resource_id"] for item in page["items"]}
        status, _, tail = shell.http("/api/tasks", {"cursor": page["next_cursor"]}, token)
        self.assertEqual(status, 200)
        self.assertTrue(tail["complete"])
        self.assertEqual(tail["snapshot_id"], page["snapshot_id"])
        self.assertEqual({item["resource_id"] for item in tail["items"]}, tasks - head)
        status, _, detail = shell.http("/api/task", {"id": representative}, token)
        self.assertEqual((status, detail), (200, completed))

        node_id = self.node.call("Node.Get", {})["reference"]["resource_id"]
        self.node.stop(signal.SIGKILL)
        self.error(shell, 503, "DependencyUnavailable", "/api/node", data={}, token=token)
        self.node.start()
        status, _, node = shell.http("/api/node", {}, token)
        self.assertEqual(status, 200)
        self.assertEqual(node["reference"]["resource_id"], node_id)
        self.assertGreater(int(node["authority_storage"]["state"]["capacity_bytes"]), 0)
        self.assertEqual(shell.http("/api/task", {"id": representative}, token)[2], completed)

    def test_history_pins_a_task_revision_across_approval_and_restart(self):
        node = self.node
        volume = node.perform("volume.create", {"name": "history"})
        task = node.submit("volume.write", {**target(volume), "name": "record", "content": "history"})
        initial = node.call("Task.Get", {"id": task})
        self.assertEqual(initial["lifecycle"], "WaitingApproval")
        shell = self.shell()
        token = shell.pair()["token"]

        def history(request):
            status, _, page = shell.http("/api/task-history", request, token)
            self.assertEqual(status, 200, page)
            return page

        pinned = {"id": task, "through_version": initial["task_revision"]}
        first = history(pinned)
        self.assertEqual(first, node.call("Task.History", {**pinned, "limit": 3}))
        node.approve(task)
        completed = node.wait(task)
        self.assertEqual(completed["lifecycle"], "Succeeded")
        self.assertEqual(history(pinned), first)

        latest = {"id": task, "through_version": completed["task_revision"]}
        page = history(latest)
        self.assertEqual(len(page["items"]), 3)
        self.assertIsNotNone(page["next_after_version"])
        items = list(page["items"])
        node.stop(signal.SIGKILL)
        self.error(shell, 503, "DependencyUnavailable", "/api/task-history", data=latest, token=token)
        node.start()
        while page["next_after_version"]:
            page = history({**latest, "after_version": page["next_after_version"]})
            items.extend(page["items"])
        self.assertEqual(items, node.call("Task.History", latest)["items"])
        self.assertEqual(items[-1]["run_id"], completed["current_run_id"])
        self.error(shell, 409, "PreconditionFailed", "/api/task-history",
                   data={**latest, "after_version": "r999"}, token=token)
        self.error(shell, 400, "InvalidArgument", "/api/task-history", data={"id": task}, token=token)
        self.error(shell, 400, "InvalidArgument", "/api/task-history", data={**latest, "limit": 128}, token=token)

    def test_scoped_reads_grant_and_identity_revocation(self):
        owner_task = self.node.submit("system.status", {})
        owner_reference = self.node.wait(owner_task)["result"]["artifact_refs"][0]
        agent, grant = self.node.agent(["resource.read", "task.read", "task.submit", "system.status"])
        credential = self.node.base / "agent.token"
        credential.touch(mode=0o600)
        credential.write_text(agent["token"])
        shell = self.shell(credential)
        session = shell.pair()
        self.assertEqual(session["principal_id"], agent["principal_id"])
        token = session["token"]
        self.error(shell, 403, "PermissionDenied", "/api/node", data={}, token=token)
        self.error(shell, 403, "PermissionDenied", "/api/task", data={"id": owner_task}, token=token)
        for download in (False, True):
            self.error(shell, 403, "PermissionDenied", "/api/task-artifact",
                       data={"task_id": owner_task, "reference": owner_reference, "download": download}, token=token)

        self.error(shell, 403, "PermissionDenied", "/api/task-history",
                   data={"id": owner_task, "through_version": "r1"}, token=token)
        delegated = self.node.prepare("system.status", {}, token=agent["token"],
                                      delegation=grant["delegation_ref"]["resource_id"])
        task = self.node.call("Task.Submit", delegated, agent["token"])["task_id"]
        self.assertEqual(self.node.wait(task)["lifecycle"], "Succeeded")
        status, _, listing = shell.http("/api/tasks", {}, token)
        self.assertEqual(status, 200)
        self.assertEqual([item["resource_id"] for item in listing["items"]], [task])
        status, _, body = shell.http("/api/task", {"id": task}, token)
        self.assertEqual((status, body["task_id"], body["lifecycle"]), (200, task, "Succeeded"))

        reference = body["result"]["artifact_refs"][0]
        download = {"task_id": task, "reference": reference, "download": True}
        status, _, raw = shell.http("/api/task-artifact", download, token)
        self.assertEqual((status, "sha256:" + hashlib.sha256(raw).hexdigest()), (200, reference["digest"]))
        history_request = {"id": task, "through_version": body["task_revision"]}
        status, _, page = shell.http("/api/task-history", history_request, token)
        self.assertEqual(status, 200)
        self.assertTrue(page["next_after_version"])
        self.node.call("Grant.Revoke", target(grant["grant_ref"]))
        self.error(shell, 403, "PermissionDenied", "/api/task-history",
                   data={**history_request, "after_version": page["next_after_version"]}, token=token)
        self.error(shell, 403, "PermissionDenied", "/api/task-artifact", data=download, token=token)
        self.error(shell, 403, "PermissionDenied", "/api/tasks", data={}, token=token)
        self.node.call("Principal.Revoke", target(self.node.get(agent["principal_id"])["reference"]))
        self.error(shell, 401, "AuthenticationRequired", "/api/node", data={}, token=token)
        self.assertEqual(shell.process.wait(timeout=5), 0)

    def large_result(self):
        volume = self.node.perform("volume.create", {"name": "large-preview"})
        content = "\0" * (48 * 1024) + "尾"
        self.node.perform("volume.write", dict(target(volume), name="large", content=content))
        current = self.node.get(volume["resource_id"])["reference"]
        task = self.node.submit("volume.read", dict(target(current), name="large"))
        reference = self.node.wait(task)["result"]["artifact_refs"][0]
        return task, reference, content

    def test_previews_only_the_exact_task_artifact_with_a_byte_limit(self):
        shell = self.shell()
        token = shell.pair()["token"]
        task = self.node.submit("system.status", {})
        reference = self.node.wait(task)["result"]["artifact_refs"][0]
        request = {"task_id": task, "reference": reference}
        self.error(shell, 401, "AuthenticationRequired", "/api/task-artifact", data=request)
        status, headers, result = shell.http("/api/task-artifact", request, token)
        self.assertEqual((status, result["reference"]), (200, reference))
        self.assertEqual(headers["Cache-Control"], "no-store")
        self.assertEqual("sha256:" + hashlib.sha256(result["content_json"].encode()).hexdigest(),
                         reference["digest"])
        self.assertEqual(json.loads(result["content_json"]), self.node.call("Artifact.Read", {"reference": reference}))

        unrelated = self.node.submit("system.status", {})
        self.node.wait(unrelated)
        self.error(shell, 404, "NotFound", "/api/task-artifact", data=dict(request, task_id=unrelated), token=token)
        wrong = dict(reference, digest="sha256:" + "0" * 64)
        self.error(shell, 404, "NotFound", "/api/task-artifact", data=dict(request, reference=wrong), token=token)

        task, reference, content = self.large_result()
        self.error(shell, 413, "QuotaExceeded", "/api/task-artifact",
                   data={"task_id": task, "reference": reference}, token=token)
        download = {"task_id": task, "reference": reference, "download": True}
        self.error(shell, 401, "AuthenticationRequired", "/api/task-artifact", data=download)
        self.error(shell, 404, "NotFound", "/api/task-artifact", data=dict(download, task_id=unrelated), token=token)
        status, headers, raw = shell.http("/api/task-artifact", download, token)
        self.assertEqual((status, headers["Content-Type"], headers["Cache-Control"]),
                         (200, "application/octet-stream", "no-store"))
        self.assertIn("attachment", headers["Content-Disposition"])
        self.assertGreater(len(raw), 2 * 128 * 1024)
        self.assertEqual("sha256:" + hashlib.sha256(raw).hexdigest(), reference["digest"])
        self.assertEqual(json.loads(raw)["content"], content)

        self.node.stop()
        with closing(sqlite3.connect(self.node.state / "state.db")) as db, db:
            updated = db.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload)) "
                                 "WHERE artifact_id=? AND chunk_index=1", (reference["resource_id"],))
            self.assertEqual(updated.rowcount, 1)
        self.node.start()
        self.error(shell, 503, "RecoveryRequired", "/api/task-artifact", data=download, token=token)

    def relay(self, listener, methods, hold=None, waiting=None, release=None, *, drop=None):
        for method in methods:
            peer, _ = listener.accept()
            with peer, socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream:
                request = frame(peer)
                self.assertEqual(json.loads(request[4:])["method"], method)
                upstream.connect(str(self.node.socket))
                upstream.sendall(request)
                response = frame(upstream)
                if method == hold:
                    waiting.set()
                    self.assertTrue(release.wait(8))
                if method == drop:
                    drop = None
                else:
                    peer.sendall(response)

    def test_lost_cancellation_reply_is_resolved_by_reading_the_task(self):
        node = self.node
        state = node.call("Node.Get", {})["reference"]
        node.call("Node.Configure", {"expected_resource_version": state["version"], "scheduling": "Cordoned"})
        task = node.submit("system.status", {})
        pending = node.call("Task.Get", {"id": task})
        proxy = node.base / "lost-reply.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(1) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)
            forwarded = pool.submit(self.relay, listener, ("Principal.WhoAmI", "Task.Get", "Task.Cancel", "Task.Get"),
                                    drop="Task.Cancel")
            shell = Shell(node, node.witness / "owner.token", proxy, cancel_pending=True)
            self.addCleanup(shell.close)
            token = shell.pair()["token"]
            status, _, fault = shell.http("/api/cancel-pending-task",
                                          {"id": task, "expected_resource_version": pending["task_revision"]}, token)
            self.assertEqual(status, 503, fault)
            status, _, observed = shell.http("/api/task", {"id": task}, token)
            self.assertEqual((status, observed["lifecycle"], observed["outcome_knowledge"]), (200, "Cancelled", "NotStarted"))
            forwarded.result(8)
            self.assertFalse(select.select([listener], [], [], 0)[0])

    def test_task_claimed_between_cancellation_checks_keeps_its_result(self):
        n = self.node
        state = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": state["version"], "scheduling": "Cordoned"})
        task = n.submit("system.status", {})
        pending = n.call("Task.Get", {"id": task})
        self.assertEqual(pending["lifecycle"], "Queued")
        proxy = n.base / "cancel-race.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)
            forwarded = pool.submit(self.relay, listener, ("Principal.WhoAmI", "Task.Get", "Task.Cancel"),
                                    "Task.Get", waiting, release)
            shell = Shell(n, n.witness / "owner.token", proxy, cancel_pending=True)
            self.addCleanup(shell.close)
            token = shell.pair()["token"]
            request = {"id": task, "expected_resource_version": pending["task_revision"]}
            cancel = pool.submit(shell.http, "/api/cancel-pending-task", request, token)
            try:
                self.assertTrue(waiting.wait(8))
                state = n.call("Node.Get", {})["reference"]
                n.call("Node.Configure", {"expected_resource_version": state["version"], "scheduling": "Enabled"})
                done = n.wait(task)
                self.assertEqual(done["lifecycle"], "Succeeded")
            finally:
                release.set()
            forwarded.result(8)
            status, _, fault = cancel.result(8)
            self.assertEqual((status, fault["error"]["code"]), (409, "Conflict"))
            self.assertEqual(n.call("Task.Get", {"id": task}), done)

    def test_logout_during_download_stops_before_the_next_core_chunk(self):
        task, reference, _ = self.large_result()
        proxy = self.node.base / "shell-delivery.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)

            forwarding = pool.submit(self.relay, listener, ("Principal.WhoAmI", "Task.Get", "Artifact.ReadChunk"),
                                     "Artifact.ReadChunk", waiting, release)
            shell = Shell(self.node, self.node.witness / "owner.token", proxy)
            self.addCleanup(shell.close)
            token = shell.pair()["token"]
            download = pool.submit(shell.http, "/api/task-artifact",
                                   {"task_id": task, "reference": reference, "download": True}, token)
            try:
                self.assertTrue(waiting.wait(8))
                self.assertEqual(shell.http("/api/logout", {}, token)[0], 200)
            finally:
                release.set()
            forwarding.result(8)
            status, headers, error = download.result(8)
            self.assertEqual((status, error["error"]["code"]), (401, "AuthenticationRequired"))
            self.assertIsNone(headers["Content-Disposition"])
            self.assertEqual(shell.process.wait(timeout=5), 0)
            self.assertFalse(select.select([listener], [], [], 0)[0], "download requested another Core chunk")
