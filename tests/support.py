"""Black-box harness. Only the API, Linux process signals and the test's own state are used."""
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import uuid


class Failure(Exception):
    def __init__(self, response):
        super().__init__(response)
        self.code = response["code"]


def key():
    return uuid.uuid4().hex


class Node:
    def __init__(self, binaries, tick_ms=15):
        self.tmp = tempfile.TemporaryDirectory(prefix="psios-test-")
        self.base = Path(self.tmp.name)
        self.state = self.base / "state"
        self.witness = self.base / "witness"
        self.socket = self.base / "api.sock"
        self.binaries = Path(binaries).absolute()
        self.config = self.base / "config.json"
        self.config.write_text(json.dumps(dict(state_dir=str(self.state), witness_dir=str(self.witness),
                                               socket=str(self.socket), tick_ms=tick_ms, queue_limit=64)))
        self.daemon = [str(self.binaries / "psiosd"), "--config", str(self.config)]
        subprocess.run(self.daemon + ["init"], check=True, stdout=subprocess.DEVNULL)
        self.owner = (self.witness / "owner.token").read_text()
        self.process = None
        self.start()

    def start(self):
        self.process = subprocess.Popen(self.daemon + ["serve"])
        for _ in range(300):
            if self.process.poll() is not None:
                raise AssertionError("daemon failed to start")
            try:
                self.call("Principal.WhoAmI", {})
                return
            except (FileNotFoundError, ConnectionError):
                time.sleep(0.02)
        raise AssertionError("daemon readiness timeout")

    def stop(self, sig=signal.SIGTERM):
        if self.process and self.process.poll() is None:
            self.process.send_signal(sig)
            code = self.process.wait(25)
            assert code == (0 if sig == signal.SIGTERM else -sig), code

    def close(self):
        try:
            self.stop()
        finally:
            self.tmp.cleanup()

    def call(self, method, params, token=None):
        request_id = key()
        request = json.dumps(dict(schema_version="1.0.0", token=self.owner if token is None else token,
                                  request_id=request_id, method=method, params=params)).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(20)
            stream.connect(str(self.socket))
            stream.sendall(struct.pack(">I", len(request)) + request)
            with stream.makefile("rb") as f:
                length, = struct.unpack(">I", f.read(4))
                result = json.loads(f.read(length))
        assert result["request_id"] == request_id
        if "error" in result:
            raise Failure(result["error"])
        return result["result"]

    def get(self, resource):
        return self.call("Resource.Get", {"id": resource})

    def prepare(self, tool, params, submission_key=None, token=None, delegation="owner-delegation"):
        return self.call("Task.Prepare", dict(tool=tool, input=params, key=submission_key or key(),
                                              delegation_id=delegation), token)

    def submit(self, tool, params):
        return self.call("Task.Submit", self.prepare(tool, params))["task_id"]

    def approve(self, task_id, approve=True):
        task = self.call("Task.Get", {"id": task_id})
        assert task["lifecycle"] == "WaitingApproval", task
        req = self.get(task["conditions"][0]["message"])
        return self.call("Approval.Decide", dict(id=req["reference"]["resource_id"],
                         expected_resource_version=req["reference"]["version"],
                         plan_digest=req["value"]["plan_digest"], approve=approve))

    def wait(self, task_id, autoapprove=False):
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            task = self.call("Task.Get", dict(id=task_id))
            state = task["lifecycle"]
            if state == "WaitingApproval" and autoapprove:
                self.approve(task_id)
            elif state in ("WaitingApproval", "Failed", "Paused", "Cancelled", "Succeeded"):
                return task
            time.sleep(0.035)
        raise AssertionError(task)

    def perform(self, tool, params):
        task = self.wait(self.submit(tool, params), autoapprove=True)
        assert task["lifecycle"] == "Succeeded", task
        return self.call("Artifact.Read", dict(reference=task["result"]["artifact_refs"][0]))

    def cli(self, token_file, *args):
        return json.loads(subprocess.check_output([self.binaries / "psiosctl", "--socket", self.socket,
                                                   "--token-file", token_file, *args]))

    def agent(self, actions, units=20):
        a = self.call("Agent.Register", {"name": "test-worker"})
        g = self.call("Grant.Issue", dict(principal_id=a["principal_id"], actions=actions,
                                          units=units, ttl_seconds=600))
        return a, g


def target(reference):
    return dict(id=reference["resource_id"], expected_resource_version=reference["version"])
