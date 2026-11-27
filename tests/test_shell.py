"""Exercise the local browser boundary against a real psiosd process."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import socket
import subprocess
import unittest
from threading import Event
from urllib.parse import urlsplit

from model_delivery import frame
from support import Node, target


class Shell:
    def __init__(self, node, token_file, endpoint=None):
        self.process = subprocess.Popen(
            [node.binaries / "psiosctl", "--socket", endpoint or node.socket, "--token-file", token_file, "shell"],
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
        result = self.node.wait(task)
        self.assertEqual(result["lifecycle"], "Succeeded")
        digest = result["result"]["artifact_refs"][0]["digest"]
        downloads = self.node.base / "downloads"
        downloads.mkdir(mode=0o700)
        large_task, large_ref, _ = self.large_result()
        for _ in range(18):
            queued = self.node.submit("system.status", {})
            self.assertEqual(self.node.wait(queued)["lifecycle"], "Succeeded")
        shell = self.shell()
        browser = os.environ.get("PSIOS_TEST_CHROME", "google-chrome")
        subprocess.run(
            ["node", str(Path(__file__).with_name("shell_browser.mjs")), browser],
            input=json.dumps(dict(origin=shell.origin, code=shell.code, task=task, digest=digest,
                                  large_task=large_task, large_digest=large_ref["digest"], downloads=str(downloads),
                                  node=self.node.call("Node.Get", {})["reference"]["resource_id"],
                                  profile=str(self.node.base / "browser"))),
            text=True, timeout=110, check=True,
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
        self.node.call("Grant.Revoke", target(grant["grant_ref"]))
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

    def test_logout_during_download_stops_before_the_next_core_chunk(self):
        task, reference, _ = self.large_result()
        proxy = self.node.base / "shell-delivery.sock"
        waiting, release = Event(), Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener, ThreadPoolExecutor(2) as pool:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)

            def relay():
                for method in ("Principal.WhoAmI", "Task.Get", "Artifact.ReadChunk"):
                    peer, _ = listener.accept()
                    with peer, socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream:
                        request = frame(peer)
                        self.assertEqual(json.loads(request[4:])["method"], method)
                        upstream.connect(str(self.node.socket))
                        upstream.sendall(request)
                        response = frame(upstream)
                        if method == "Artifact.ReadChunk":
                            waiting.set()
                            self.assertTrue(release.wait(8))
                        peer.sendall(response)

            forwarding = pool.submit(relay)
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
