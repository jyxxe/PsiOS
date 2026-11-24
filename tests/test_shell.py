"""Exercise the local browser boundary against a real psiosd process."""
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import unittest
from urllib.parse import urlsplit

from support import Node, target


class Shell:
    def __init__(self, node, token_file):
        self.process = subprocess.Popen(
            [node.binaries / "psiosctl", "--socket", node.socket, "--token-file", token_file, "shell"],
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

    def shell(self, token_file=None):
        shell = Shell(self.node, token_file or self.node.witness / "owner.token")
        self.addCleanup(shell.close)
        return shell

    def error(self, shell, status, code, path, **kwargs):
        actual, headers, body = shell.http(path, **kwargs)
        self.assertEqual(actual, status)
        self.assertEqual(body["error"]["code"], code)
        self.assertEqual(headers["Cache-Control"], "no-store")
        self.assertIsNone(headers["Access-Control-Allow-Origin"])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SHELL_BROWSER"), "requires headless Chrome and Node.js")
    def test_browser_pairs_reads_tasks_and_restores_selection_focus(self):
        task = self.node.submit("system.status", {})
        self.assertEqual(self.node.wait(task)["lifecycle"], "Succeeded")
        for _ in range(21):
            queued = self.node.submit("system.status", {})
            self.assertEqual(self.node.wait(queued)["lifecycle"], "Succeeded")
        shell = self.shell()
        browser = os.environ.get("PSIOS_TEST_CHROME", "google-chrome")
        subprocess.run(
            ["node", str(Path(__file__).with_name("shell_browser.mjs")), browser],
            input=json.dumps(dict(origin=shell.origin, code=shell.code, task=task,
                                  node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                                  profile=str(self.node.base / "browser"))),
            text=True, timeout=45, check=True,
        )
        self.assertEqual(shell.process.wait(timeout=5), 0)

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

    def test_scoped_reads_grant_and_identity_revocation(self):
        owner_task = self.node.submit("system.status", {})
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

        delegated = self.node.prepare("system.status", {}, token=agent["token"],
                                      delegation=grant["delegation_ref"]["resource_id"])
        task = self.node.call("Task.Submit", delegated, agent["token"])["task_id"]
        self.assertEqual(self.node.wait(task)["lifecycle"], "Succeeded")
        status, _, listing = shell.http("/api/tasks", {}, token)
        self.assertEqual(status, 200)
        self.assertEqual([item["resource_id"] for item in listing["items"]], [task])
        status, _, body = shell.http("/api/task", {"id": task}, token)
        self.assertEqual((status, body["task_id"], body["lifecycle"]), (200, task, "Succeeded"))

        self.node.call("Grant.Revoke", target(grant["grant_ref"]))
        self.error(shell, 403, "PermissionDenied", "/api/tasks", data={}, token=token)
        self.node.call("Principal.Revoke", target(self.node.get(agent["principal_id"])["reference"]))
        self.error(shell, 401, "AuthenticationRequired", "/api/node", data={}, token=token)
        self.assertEqual(shell.process.wait(timeout=5), 0)
