"""Exercise real local inference transport failures through the durable provider CLI."""
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler
import json
import os
import signal
import socketserver
import subprocess
import threading
import time
import unittest

from model_delivery import interrupt_worker_delivery
from support import Failure, Node, key, target


class LocalInferenceHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        size = int(self.headers["Content-Length"])
        assert self.path == "/v1/chat/completions" and 0 < size <= 256 * 1024
        self.server.requests.append(json.loads(self.rfile.read(size)))
        if self.server.gate is not None:
            received, release = self.server.gate
            received.set()
            assert release.wait(12), "inference test worker never released its reply gate"
        if self.server.drop_reply:
            self.close_connection = True
            return
        decision = json.dumps(self.server.decision)
        response = {"model": "strict-fixture", "choices": [
            {"finish_reason": "stop", "message": {"role": "assistant", "content": decision}},
        ]}
        if self.server.usage is not None:
            response["usage"] = self.server.usage
        data = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args):
        pass


@contextmanager
def serve_inference(endpoint, *, drop_reply=False, gate=None, decision=None, usage=None):
    with socketserver.UnixStreamServer(str(endpoint), LocalInferenceHandler) as service:
        service.requests = []
        service.drop_reply = drop_reply
        service.gate = gate
        service.decision = decision if decision is not None else {
            "action": "finish", "summary": "Recovered with a fresh authorized call",
        }
        service.usage = usage
        thread = threading.Thread(target=service.serve_forever, kwargs={"poll_interval": 0.05})
        thread.start()
        try:
            yield service.requests
        finally:
            if gate is not None:
                gate[1].set()
            service.shutdown()
            thread.join(2)
            assert not thread.is_alive()
    endpoint.unlink()


class StrictModelProvider(unittest.TestCase):
    lease_seconds = 8

    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.provider, _ = self.node.agent(["model.serve"])
        self.owner = self.node.call("Agent.Register", {"name": "strict-model-owner"})
        self.grant = self.node.call("Grant.Issue", {
            "principal_id": self.owner["principal_id"], "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "model_calls": 4, "model_lease_ms": 4 * self.lease_seconds * 1000,
            "units": 4, "ttl_seconds": 180,
        })
        self.model = self.node.call("Model.Register", {
            "name": "strict-fixture", "revision": "1", "provider_principal_id": self.provider["principal_id"],
            "inference_outcome_policy": "Strict", "lease_seconds": self.lease_seconds, "max_in_flight": 1,
            "max_context_bytes": 16 * 1024, "max_output_bytes": 2048,
        })
        self.skill = self.node.call("Skill.Publish", {"name": "strict-result", "version": "1", "license": "0BSD",
            "source": "local-test", "tools": ["system.status"], "instructions": "Provide a final summary."})
        self.provider_token = self.credential("provider.token", self.provider["token"])
        self.owner_token = self.credential("agent.token", self.owner["token"])
        self.journal = self.node.base / "strict-journal"
        self.journal.mkdir(mode=0o700)
        self.model_config = self.node.base / "strict-model.json"
        self.endpoint = self.node.base / "inference.sock"
        self.model_config.write_text(json.dumps({"socket": str(self.endpoint), "server_uid": os.geteuid(),
            "model_ref": self.model, "model_alias": "strict-fixture", "max_tokens": 112, "timeout_ms": 1500}))

    def credential(self, name, token):
        path = self.node.base / name
        path.write_text(token)
        path.chmod(0o600)
        return path

    def open_session(self, *, queued=False):
        return self.node.call("Session.Open", {"key": key(), "goal": "Summarize the current state.",
            "model_ref": self.model, "skill_ref": self.skill, "context_policy": "ReferencesOnly", "max_turns": 3,
            "delegation_id": self.grant["delegation_ref"]["resource_id"], "provider_queue": queued},
            self.owner["token"])["reference"]["resource_id"]

    def step(self, session, request_key):
        return ["model-step", session, "--key", request_key, "--decision-dir", self.journal,
                "--local-model", self.model_config]

    def history(self, session):
        return self.node.call("Session.History", {"id": session}, self.owner["token"])

    def budget(self):
        return self.node.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]}, self.owner["token"])

    def test_pre_send_failure_replays_the_report_then_resumes_with_a_fresh_call(self):
        node = self.node
        session = self.open_session()
        command = self.step(session, "absent-inference")
        with interrupt_worker_delivery(node, self.provider_token, command, committed=True,
                                       method="Model.ReportFailure"):
            saved = json.loads((self.journal / "psios-model-pending.json").read_bytes())
            self.assertEqual(saved["format"], 3)
        node.stop(signal.SIGKILL)
        node.start()
        with serve_inference(self.endpoint) as calls:
            failure = node.cli(self.provider_token, *command)
            self.assertEqual((failure["outcome"], failure["lifecycle"], failure["outcome_knowledge"]),
                             ("NotStarted", "Paused", "NotStarted"))
            self.assertEqual(calls, [])
            self.assertEqual(list(self.journal.iterdir()), [])
            self.assertEqual(self.budget()["model_calls_used"], 1)
            self.assertLess(self.budget()["model_lease_ms_charged"], self.lease_seconds * 1000)
            self.assertEqual(self.history(session)["tasks"], [])

            node.cli(self.owner_token, "session-resume", session,
                     "--revision", failure["session_ref"]["version"])
            finished = node.cli(self.provider_token, *self.step(session, "resumed-inference"))
            self.assertEqual(finished["lifecycle"], "Succeeded")
            self.assertEqual(len(calls), 1)
        self.assertEqual(self.budget()["model_calls_used"], 2)
        self.assertEqual(len(self.history(session)["turns"]), 2)
        self.assertEqual(self.history(session)["tasks"], [])

    def test_lost_http_reply_remains_unknown_and_never_dispatches_again(self):
        node = self.node
        session = self.open_session()
        queued = self.open_session(queued=True)
        with serve_inference(self.endpoint, drop_reply=True) as calls:
            failure = node.cli(self.provider_token, *self.step(session, "lost-inference"))
            self.assertEqual((failure["outcome"], failure["lifecycle"], failure["outcome_knowledge"]),
                             ("Unknown", "Paused", "Unknown"))
            self.assertEqual(len(calls), 1)
            self.assertEqual(self.budget()["model_lease_ms_charged"], self.lease_seconds * 1000)
            queued_request = {"key": "wait-for-inference-slot", "model_ref": self.model}
            self.assertEqual(node.call("Model.ClaimNext", queued_request, self.provider["token"]), {"available": False})
            node.stop(signal.SIGKILL)
            node.start()

            with self.assertRaises(Failure):
                node.call("Session.Resume", target(failure["session_ref"]), self.owner["token"])
            with self.assertRaises(Failure):
                node.call("Model.Claim", {"session_id": session, "key": "try-again"}, self.provider["token"])
            self.assertEqual(node.cli(self.provider_token, *self.step(session, "lost-inference")), failure)
            self.assertEqual(self.budget()["model_calls_used"], 1)

            limit = time.monotonic() + self.lease_seconds + 2
            while True:
                claim = node.call("Model.ClaimNext", queued_request, self.provider["token"])
                if claim["available"]:
                    break
                self.assertLess(time.monotonic(), limit, "unknown lease did not release its concurrency slot")
                time.sleep(0.1)
            self.assertEqual(claim["session_id"], queued)
            self.assertEqual(len(calls), 1)
        self.assertEqual(self.history(session)["tasks"], [])
        self.assertEqual(len(self.history(session)["turns"]), 1)

    def test_worker_killed_after_post_cannot_create_another_inference_after_restart(self):
        node = self.node
        session = self.open_session()
        received, release = threading.Event(), threading.Event()
        args = self.step(session, "killed-inference")
        command = [node.binaries / "psiosctl", "--socket", node.socket, "--token-file", self.provider_token, *args]
        with serve_inference(self.endpoint, drop_reply=True, gate=(received, release)) as calls:
            with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as worker:
                try:
                    self.assertTrue(received.wait(4), "the local adapter never sent its first inference request")
                    worker.kill()
                    self.assertEqual(worker.wait(4), -signal.SIGKILL)
                finally:
                    release.set()
                    if worker.poll() is None:
                        worker.kill()
            self.assertEqual(list(self.journal.iterdir()), [])
            node.stop(signal.SIGKILL)
            node.start()
            replay = subprocess.run(command, capture_output=True, timeout=5)
            if replay.returncode != 0:
                self.assertIn(b"OutcomeUnknown", replay.stderr)
            else:
                self.assertEqual(json.loads(replay.stdout)["outcome"], "Unknown")

            limit = time.monotonic() + self.lease_seconds + 2
            while True:
                state = node.call("Session.Get", {"id": session}, self.owner["token"])["session"]
                if state["lifecycle"] == "Paused":
                    break
                self.assertLess(time.monotonic(), limit, "expired strict inference was not settled")
                time.sleep(0.1)
            self.assertEqual((state["pause_reason"], state["outcome_knowledge"]), ("ModelOutcomeUnknown", "Unknown"))
            self.assertEqual(state["model_inference_failure"]["source"], "LeaseExpired")
            self.assertEqual(len(calls), 1)
        self.assertEqual(self.history(session)["tasks"], [])
        self.assertEqual(self.budget()["model_calls_used"], 1)
        self.assertEqual(self.budget()["model_lease_ms_charged"], self.lease_seconds * 1000)
