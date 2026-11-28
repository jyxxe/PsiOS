"""Exercise real local inference transport failures through the durable provider CLI."""
from concurrent.futures import ThreadPoolExecutor
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
            {"finish_reason": self.server.finish_reason, "message": {"role": "assistant", "content": decision}},
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
def serve_inference(endpoint, *, drop_reply=False, gate=None, decision=None, usage=None, finish_reason="stop"):
    with socketserver.UnixStreamServer(str(endpoint), LocalInferenceHandler) as service:
        service.requests = []
        service.drop_reply = drop_reply
        service.gate = gate
        service.decision = decision if decision is not None else {
            "action": "finish", "summary": "Recovered with a fresh authorized call",
        }
        service.usage = usage
        service.finish_reason = finish_reason
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

    def assert_lease_bounded(self, *, queued):
        node = self.node
        lease_ms = 1200
        self.grant = node.call("Grant.Issue", {
            "principal_id": self.owner["principal_id"], "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "model_calls": 2, "model_lease_ms": lease_ms, "units": 4, "ttl_seconds": 180,
        })
        configuration = json.loads(self.model_config.read_text())
        configuration["timeout_ms"] = 8000
        self.model_config.write_text(json.dumps(configuration))
        session = self.open_session(queued=queued)
        if queued:
            model_file = node.base / "model-ref.json"
            model_file.write_text(json.dumps(self.model))
            args = ["model-worker", model_file, "--decision-dir", self.journal,
                    "--local-model", self.model_config, "--wait-ms", "3000", "--once"]
        else:
            args = self.step(session, "actual-lease-timeout")
        command = [node.binaries / "psiosctl", "--socket", node.socket, "--token-file", self.provider_token, *args]
        received, release = threading.Event(), threading.Event()
        with serve_inference(self.endpoint, drop_reply=True, gate=(received, release)) as calls:
            started = time.monotonic()
            worker = subprocess.run(command, capture_output=True, text=True, timeout=4)
            elapsed = time.monotonic() - started
            self.assertTrue(received.is_set(), worker.stderr)
            # The provider timeout and Core watchdog use the same lease deadline.
            # A report that reaches Core after expiry is fenced. The queued CLI
            # returns that disposition while the direct CLI reports it as an error.
            if worker.returncode == 0:
                receipt = json.loads(worker.stdout)
                fenced = queued and receipt == {"status": "Fenced"}
            else:
                self.assertFalse(queued, worker.stderr)
                self.assertEqual(worker.returncode, 1, worker.stderr)
                self.assertTrue(worker.stderr.startswith("Fenced:"), worker.stderr)
                receipt, fenced = None, True
            self.assertGreater(elapsed, 0.4)
            self.assertLess(elapsed, 3)
            self.assertEqual(len(calls), 1)
            prompt = calls[0]["messages"][1]["content"]
            self.assertNotIn("lease_timing", prompt)

        token = self.owner["token"]
        observed = node.call("Session.Get", {"id": session}, token)
        if observed["session"]["lifecycle"] == "ModelInFlight":
            observed = node.call("Session.Wait", {"id": session, "after_version": observed["reference"]["version"],
                                                  "timeout_ms": 1500}, token)
        status = observed["session"]
        self.assertEqual((status["lifecycle"], status["outcome_knowledge"]), ("Paused", "Unknown"))
        outcome = status["model_inference_failure"]
        self.assertEqual(outcome["source"], "LeaseExpired" if fenced else "Provider")
        usage = node.call("Session.ModelUsage", {"id": session}, token)
        self.assertEqual(usage["session_ref"], observed["reference"])
        self.assertEqual(usage["calls"], [{"turn": 1, "lease_id": outcome["lease_id"], "outcome": "Unknown",
                                           "charged_lease_ms": lease_ms, "provider_reported_tokens": None}])
        self.assertNotIn(outcome["lease_id"], prompt)
        if not fenced:
            self.assertEqual(receipt["session_ref"], observed["reference"])
            self.assertEqual(receipt["lease_id"], outcome["lease_id"])
            self.assertEqual((receipt["lifecycle"], receipt["outcome_knowledge"]), ("Paused", "Unknown"))
        self.assertEqual(self.budget()["model_calls_used"], 1)
        self.assertEqual(self.budget()["model_lease_ms_charged"], lease_ms)
        self.assertEqual(self.history(session)["tasks"], [])
        self.assertEqual(list(self.journal.iterdir()), [])

    def test_direct_transport_stops_before_the_configured_timeout_when_core_grants_less(self):
        self.assert_lease_bounded(queued=False)

    def test_queued_transport_uses_the_same_shortened_lease(self):
        self.assert_lease_bounded(queued=True)

    def test_session_wait_observes_commits_and_times_out_at_the_current_revision(self):
        node = self.node
        session = self.open_session()
        token = self.owner["token"]
        initial = node.call("Session.Get", {"id": session}, token)
        after = initial["reference"]["version"]
        query = {"id": session, "after_version": after}
        self.assertEqual(node.call("Session.Wait", dict(query, timeout_ms=80), token), initial)
        for extra, code in (({"after_version": "r0001"}, "InvalidArgument"),
                            ({"after_version": "r999999"}, "PreconditionFailed"),
                            ({"timeout_ms": 10001}, "InvalidArgument")):
            with self.subTest(extra=extra), self.assertRaises(Failure) as refused:
                node.call("Session.Wait", {**query, "timeout_ms": 80, **extra}, token)
            self.assertEqual(refused.exception.code, code)

        with ThreadPoolExecutor(max_workers=1) as worker:
            waiting = worker.submit(node.call, "Session.Wait", dict(query, timeout_ms=4000), token)
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.08)
            claim = node.call("Model.Claim", {"session_id": session, "key": key()}, self.provider["token"])
            issued = waiting.result(timeout=2)
        self.assertEqual(issued["session"]["lifecycle"], "ModelInFlight")
        self.assertEqual(issued["session"]["current_lease"], claim["lease_id"])
        self.assertEqual(issued, node.call("Session.Get", {"id": session}, token))
        response = {"session_id": session, "lease_id": claim["lease_id"],
                    "decision": {"action": "finish", "summary": "completed observed session"}}
        result = node.call("Model.Respond", response, self.provider["token"])
        terminal = node.cli(self.owner_token, "session-wait", session, "--wait-ms", "1000")
        self.assertEqual((terminal["reference"], terminal["session"]["lifecycle"]), (result["session_ref"], "Succeeded"))
        node.stop(signal.SIGKILL)
        node.start()
        self.assertEqual(node.cli(self.owner_token, "session-wait", session, "--after-version", after, "--wait-ms", "800"),
                         terminal)

    def test_session_wait_rechecks_read_permission_and_wakes_on_operator_pause(self):
        node = self.node
        session = self.open_session()
        token = self.owner["token"]
        params = {"id": session, "timeout_ms": 4000}
        outsider, _ = node.agent(["task.read"])
        with self.assertRaises(Failure) as unrelated:
            node.call("Session.Wait", params, outsider["token"])
        self.assertEqual(unrelated.exception.code, "PermissionDenied")

        with ThreadPoolExecutor(max_workers=1) as worker:
            waiting = worker.submit(node.call, "Session.Wait", params, token)
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.08)
            node.call("Grant.Revoke", target(self.grant["grant_ref"]))
            with self.assertRaises(Failure) as revoked:
                waiting.result(timeout=2)
            self.assertEqual(revoked.exception.code, "PermissionDenied")

        node.call("Grant.Issue", {"principal_id": self.owner["principal_id"],
                                 "actions": ["task.read"], "units": 1, "ttl_seconds": 120})
        initial = node.call("Session.Get", {"id": session}, token)
        with ThreadPoolExecutor(max_workers=1) as worker:
            waiting = worker.submit(node.call, "Session.Wait", params, token)
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.08)
            paused = node.call("Session.Pause", dict(target(initial["reference"]), reason="planned operator pause"))
            self.assertEqual(waiting.result(timeout=2), paused)

    def test_model_usage_separates_active_settled_and_unknown_charges(self):
        node = self.node
        session = self.open_session()
        params = {"id": session}
        token = self.owner["token"]
        window = self.lease_seconds * 1000

        def usage():
            return node.call("Session.ModelUsage", params, token)

        def claim():
            result = node.call("Model.Claim", {"session_id": session, "key": key()}, self.provider["token"])
            return {"session_id": session, "lease_id": result["lease_id"]}

        self.assertEqual((usage()["charged_calls"], usage()["calls"]), (0, []))
        first = claim()
        active = usage()
        self.assertEqual(active["lease_time"], {"active_reserved_ms": window, "settled_ms": 0,
                                                "retained_unknown_ms": 0, "unmetered_calls": 0})
        self.assertEqual(active["calls"], [{"turn": 1, "lease_id": first["lease_id"], "outcome": "Active",
                                           "charged_lease_ms": window, "provider_reported_tokens": None}])
        node.stop(signal.SIGKILL)
        node.start()
        self.assertEqual(usage(), active)

        reported = {"input_tokens": 12, "output_tokens": 3}
        response = dict(first, decision={"action": "tool", "tool": "system.status", "input": {}}, usage=reported)
        confirmed = node.call("Model.Respond", response, self.provider["token"])
        self.assertEqual(node.wait(confirmed["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(node.call("Model.Respond", response, self.provider["token"]), confirmed)
        first_ms = node.call("Resource.Resolve", {"reference": confirmed["usage_ref"]})["value"]["authority_elapsed_lease_ms"]

        second = claim()
        node.call("Model.ReportFailure", dict(second, model_ref=self.model, outcome="NotStarted"), self.provider["token"])
        status = node.call("Session.Get", params, token)
        node.call("Session.Resume", target(status["reference"]), token)
        third = claim()
        current = node.call("Session.Get", params, token)
        node.call("Session.Cancel", target(current["reference"]), token)
        result = usage()
        self.assertEqual(result["charged_calls"], 3)
        self.assertEqual(result["budget_ref"], self.grant["budget_ref"])
        self.assertEqual(result["session_ref"], node.call("Session.Get", params, token)["reference"])
        self.assertEqual([call["outcome"] for call in result["calls"]], ["Responded", "NotStarted", "Unknown"])
        self.assertEqual([call["lease_id"] for call in result["calls"]],
                         [claim["lease_id"] for claim in (first, second, third)])
        self.assertEqual([call["provider_reported_tokens"] for call in result["calls"]], [reported, None, None])
        self.assertEqual(result["calls"][0]["charged_lease_ms"], first_ms)
        second_ms = result["calls"][1]["charged_lease_ms"]
        self.assertGreaterEqual(second_ms, 1)
        self.assertLess(second_ms, window)
        self.assertEqual(result["calls"][2]["charged_lease_ms"], window)
        self.assertEqual(result["lease_time"], {"active_reserved_ms": 0, "settled_ms": first_ms + second_ms,
                                                "retained_unknown_ms": window, "unmetered_calls": 0})
        self.assertEqual(result["provider_reported"], dict(reported, reported_calls=1))
        self.assertEqual(self.budget()["model_lease_ms_charged"], first_ms + second_ms + window)
        self.assertEqual(node.cli(self.owner_token, "session-model-usage", session), result)
        self.assertEqual(usage(), result)

        other, _ = node.agent(["task.read"])
        with self.assertRaises(Failure) as unrelated:
            node.call("Session.ModelUsage", params, other["token"])
        self.assertEqual(unrelated.exception.code, "PermissionDenied")
        node.call("Grant.Revoke", target(self.grant["grant_ref"]))
        with self.assertRaises(Failure) as revoked:
            usage()
        self.assertEqual(revoked.exception.code, "PermissionDenied")
        node.call("Grant.Issue", {"principal_id": self.owner["principal_id"], "actions": ["task.read"],
                                 "units": 1, "ttl_seconds": 120})
        self.assertEqual(usage(), result)

    def test_active_lease_status_checks_exact_provider_and_current_consumer_scope(self):
        node = self.node
        session = self.open_session()
        claim = node.call("Model.Claim", {"session_id": session, "key": key()}, self.provider["token"])
        request = {"session_id": session, "lease_id": claim["lease_id"], "model_ref": self.model}
        self.assertEqual(node.call("Model.LeaseStatus", request, self.provider["token"]), {"status": "Active"})
        unrelated, _ = node.agent(["model.serve"])
        with self.assertRaises(Failure) as wrong_provider:
            node.call("Model.LeaseStatus", request, unrelated["token"])
        self.assertEqual(wrong_provider.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as wrong_lease:
            node.call("Model.LeaseStatus", {**request, "lease_id": key()}, self.provider["token"])
        self.assertEqual(wrong_lease.exception.code, "NotFound")
        node.call("Grant.Revoke", target(self.grant["grant_ref"]))
        self.assertEqual(node.call("Model.LeaseStatus", request, self.provider["token"]), {"status": "Ended"})
        node.stop(signal.SIGKILL)
        node.start()
        self.assertEqual(node.call("Model.LeaseStatus", request, self.provider["token"]), {"status": "Ended"})
        with self.assertRaises(Failure) as history_denied:
            self.history(session)
        self.assertEqual(history_denied.exception.code, "PermissionDenied")
        node.call("Grant.Issue", {"principal_id": self.owner["principal_id"], "actions": ["task.read"],
                                  "units": 2, "ttl_seconds": 60})
        self.assertEqual(self.history(session)["tasks"], [])

    def test_session_cancel_stops_stream_wait_before_the_remaining_timeout(self):
        node = self.node
        session = self.open_session()
        configuration = json.loads(self.model_config.read_text())
        self.model_config.write_text(json.dumps({**configuration, "timeout_ms": 8000}))
        args = self.step(session, "cancel-blocked-model-transport")
        command = [node.binaries / "psiosctl", "--socket", node.socket, "--token-file", self.provider_token, *args]
        received, release = threading.Event(), threading.Event()
        with serve_inference(self.endpoint, drop_reply=True, gate=(received, release)) as calls:
            with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as worker:
                try:
                    self.assertTrue(received.wait(4), "provider did not send the inference request")
                    current = node.call("Session.Get", {"id": session}, self.owner["token"])["reference"]
                    started = time.monotonic()
                    node.call("Session.Cancel", target(current), self.owner["token"])
                    output, error = worker.communicate(timeout=3)
                    self.assertLess(time.monotonic() - started, 3)
                finally:
                    release.set()
                    if worker.poll() is None:
                        worker.kill()
            if worker.returncode == 0:
                self.assertEqual(json.loads(output)["outcome"], "Unknown", output)
            else:
                self.assertIn("Fenced", error)
            self.assertEqual(len(calls), 1)
        state = node.call("Session.Get", {"id": session}, self.owner["token"])["session"]
        self.assertEqual((state["lifecycle"], state["outcome_knowledge"]), ("Cancelled", "Unknown"), state)
        self.assertEqual(state["model_inference_failure"]["source"], "SessionCancelled")
        self.assertEqual(self.history(session)["tasks"], [])
        self.assertEqual(self.budget()["model_calls_used"], 1)

    def test_pre_send_failure_replays_the_report_then_resumes_with_a_fresh_call(self):
        node = self.node
        session = self.open_session()
        command = self.step(session, "absent-inference")
        with interrupt_worker_delivery(node, self.provider_token, command, committed=True,
                                       method="Model.ReportFailure"):
            saved = json.loads((self.journal / "psios-model-pending.json").read_bytes())["record"]
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

    def test_empty_terminal_response_keeps_one_unknown_receipt_across_restart(self):
        node = self.node
        session = self.open_session()
        command = self.step(session, "empty-terminal")
        with serve_inference(self.endpoint, decision={"action": "finish", "summary": ""}) as calls:
            receipt = node.cli(self.provider_token, *command)
            self.assertEqual((receipt["outcome"], receipt["lifecycle"], receipt["outcome_knowledge"]),
                             ("Unknown", "Paused", "Unknown"))
            self.assertEqual(len(calls), 1)
            self.assertEqual(list(self.journal.iterdir()), [])
            node.stop(signal.SIGKILL)
            node.start()
            self.assertEqual(node.cli(self.provider_token, *command), receipt)
            self.assertEqual(len(calls), 1)
        self.assertEqual(self.history(session)["tasks"], [])
        self.assertEqual(self.budget()["model_calls_used"], 1)

    def test_equivalent_sessions_share_prompt_and_truncation_is_never_resent(self):
        node = self.node
        completed = self.open_session()
        interrupted = self.open_session()
        with serve_inference(self.endpoint) as calls:
            receipt = node.cli(self.provider_token, *self.step(completed, "complete"))
            self.assertEqual(receipt["lifecycle"], "Succeeded")
            self.assertEqual(len(calls), 1)
            original_request = calls[0]
            prompt_text = original_request["messages"][1]["content"]
            self.assertEqual(json.loads(prompt_text), {
                "goal": "Summarize the current state.", "instructions": "Provide a final summary.",
                "permitted_tools": ["system.status"], "last_tool": None, "previous_tools": [],
            })
            for value in (completed, self.skill["resource_id"], self.model["resource_id"]):
                self.assertNotIn(value, prompt_text)

        command = self.step(interrupted, "truncated")
        with serve_inference(self.endpoint, finish_reason="length") as calls:
            failure = node.cli(self.provider_token, *command)
            self.assertEqual((failure["outcome"], failure["lifecycle"], failure["outcome_knowledge"]),
                             ("Unknown", "Paused", "Unknown"))
            self.assertEqual(calls, [original_request])
            node.stop(signal.SIGKILL)
            node.start()
            self.assertEqual(node.cli(self.provider_token, *command), failure)
            self.assertEqual(calls, [original_request])
        self.assertEqual(self.history(interrupted)["tasks"], [])
        self.assertEqual(len(self.history(interrupted)["turns"]), 1)
        self.assertEqual(self.budget()["model_calls_used"], 2)

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
