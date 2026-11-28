"""Exercise the local browser boundary against a real psiosd process."""
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from contextlib import closing
import hashlib
from http.client import HTTPConnection
import io
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
from support import Node, key, target


class Shell:
    def __init__(self, node, token_file, endpoint=None, *, cancel_pending=False):
        self.process = subprocess.Popen(
            [node.binaries / "psiosctl", "--socket", endpoint or node.socket, "--token-file", token_file, "shell",
             *(["--cancel-pending-tasks"] if cancel_pending else [])],
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

    def shell(self, token_file=None, *, cancel_pending=False):
        shell = Shell(self.node, token_file or self.node.witness / "owner.token", cancel_pending=cancel_pending)
        self.addCleanup(shell.close)
        return shell

    def error(self, shell, status, code, path, **kwargs):
        actual, headers, body = shell.http(path, **kwargs)
        self.assertEqual(actual, status)
        self.assertEqual(body["error"]["code"], code)
        self.assertEqual(headers["Cache-Control"], "no-store")
        self.assertIsNone(headers["Access-Control-Allow-Origin"])

    def test_chat_directory_requires_the_paired_principal_scope(self):
        shell = self.shell()
        self.error(shell, 401, "AuthenticationRequired", "/api/conversations", data={})
        token = shell.pair()["token"]
        self.error(shell, 403, "PermissionDenied", "/api/conversations", data={}, token=token)
        self.error(shell, 400, "InvalidArgument", "/api/conversation-messages",
                   data={"id": "room-test", "limit": 32}, token=token)
        self.error(shell, 404, "NotFound", "/api/conversation-send", data={}, token=token)

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
        self.check_window_restoration(covered=False)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_revealing_a_covered_app_rechecks_its_grant_without_losing_the_maximized_task(self):
        self.check_window_restoration(covered=True)

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

    def check_window_restoration(self, *, covered):
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
                        profile=str(node.base / "browser"), control_fd=child.fileno(), covered=covered)
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
                if method != drop:
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
