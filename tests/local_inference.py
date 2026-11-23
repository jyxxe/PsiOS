"""Run a real CPU language model through the authenticated PsiOS Tool Gateway."""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

from support import Node, key, target

PINS = json.loads((Path(__file__).resolve().parent / "fixtures/local-inference.json").read_text())


class LocalHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=3)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def ready(server, path):
    limit = time.monotonic() + 60
    while time.monotonic() < limit:
        if server.poll() is not None:
            raise AssertionError("real inference server exited during loading")
        conn = LocalHTTP(path)
        try:
            conn.request("GET", "/health")
            response = conn.getresponse()
            if response.status == 200 and json.load(response)["status"] == "ok":
                return
        except (OSError, http.client.HTTPException):
            pass
        finally:
            conn.close()
        time.sleep(0.1)
    raise AssertionError("inference server readiness timeout")


def exercise(node, endpoint, work):
    provider = node.call("Agent.Register", {"name": "local-cpu-inference"})
    agent = node.call("Agent.Register", {"name": "local-inventory"})
    node.call("Grant.Issue", dict(principal_id=provider["principal_id"], actions=["model.serve"], units=1, ttl_seconds=300))
    grant = node.call("Grant.Issue", dict(principal_id=agent["principal_id"], units=8, model_calls=8,
                      actions=["session.run", "task.submit", "task.read", "node.inventory"], ttl_seconds=300))
    model = node.call("Model.Register", dict(name="SmolLM2-135M-Instruct", revision="Q4_K_M",
                      provider_principal_id=provider["principal_id"], lease_seconds=60, max_context_bytes=16384,
                      max_output_bytes=4096, max_in_flight=8))
    skill = node.call("Skill.Publish", dict(name="read-local-inventory", version="1", tools=["node.inventory"],
                     instructions="Call node.inventory with an empty input to obtain the local hardware inventory.",
                     license="MIT", source="operator"))
    config = dict(socket=str(endpoint), server_uid=os.geteuid(), model_ref=model, model_alias="psios-small",
                  max_tokens=112, timeout_ms=30000)
    cfg = work / "local-model.json"
    credential = work / "provider.token"
    credential.write_text(provider["token"])
    credential.chmod(0o600)
    journal = work / "journal"
    journal.mkdir(mode=0o700)

    def start():
        result = node.call("Session.Open", dict(key=key(), goal="Call node.inventory now with an empty input object.",
                           skill_ref=skill, model_ref=model, delegation_id=grant["delegation_ref"]["resource_id"],
                           context_policy="ReferencesOnly", max_turns=2), agent["token"])
        return result["reference"]["resource_id"]

    def step(sid, settings):
        cfg.write_text(json.dumps(settings))
        return subprocess.run([node.binaries / "psiosctl", "--socket", node.socket, "--token-file", credential,
                               "model-step", sid, "--key", "first", "--decision-dir", journal, "--local-model", cfg],
                              capture_output=True, text=True, timeout=45)

    # A real listening Unix socket withholds its response. The deadline applies
    # even when connection establishment has already succeeded.
    stalled = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stalled.bind(str(work / "stalled.sock"))
    stalled.listen()
    try:
        sid = start()
        timed = step(sid, dict(config, socket=str(work / "stalled.sock"), timeout_ms=250))
        assert timed.returncode != 0 and "DeadlineExceeded" in timed.stderr, timed.stderr
        assert not node.call("Session.History", {"id": sid}, agent["token"])["tasks"]
        current = node.call("Session.Get", {"id": sid}, agent["token"])
        node.call("Session.Cancel", target(current["reference"]), agent["token"])
    finally:
        stalled.close()
    for settings, error in [(dict(config, server_uid=(os.geteuid()+1) % 2**32), "PermissionDenied"),
                            (dict(config, model_ref=dict(model, version="r999")), "PreconditionFailed"),
                            (dict(config, socket=str(work / "absent.sock")), "DependencyUnavailable"),
                            (dict(config, max_tokens=8), "PreconditionFailed")]:
        sid = start()
        result = step(sid, settings)
        assert result.returncode != 0 and error in result.stderr, (error, result.stderr)
        assert not node.call("Session.History", {"id": sid}, agent["token"])["tasks"]
        current = node.call("Session.Get", {"id": sid}, agent["token"])
        node.call("Session.Cancel", target(current["reference"]), agent["token"])
    sid = start()
    result = step(sid, config)
    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)
    assert "task_id" in receipt, receipt
    task = node.wait(receipt["task_id"])
    assert task["lifecycle"] == "Succeeded", task
    run = node.call("Run.Get", {"id": task["current_run_id"]})
    snapshot = node.call("Resource.Resolve", {"reference": run["execution_snapshot_ref"]})["value"]
    assert snapshot["tool_refs"][0]["resource_id"] == "tool-node.inventory" and snapshot["model_ref"] == model
    output = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, agent["token"])
    assert output["boot_id"] == Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    assert output["memory_bytes"]["MemTotal"] > 0
    # A saved receipt can be queried without contacting an inference service again.
    replay = step(sid, dict(config, socket=str(work / "absent.sock")))
    assert replay.returncode == 0 and json.loads(replay.stdout) == receipt
    assert not list(journal.iterdir())
    history = node.call("Session.History", {"id": sid}, agent["token"])
    assert len(history["turns"]) == len(history["tasks"]) == 1
    budget = node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, agent["token"])
    assert (budget["model_calls_used"], budget["spent"]) == (6, 1), budget
    print("Verified real SmolLM2 CPU inference, UID and model binding, offline failure, one authorized"
          f" node.inventory Task {receipt['task_id']} and deduplicated receipt replay.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--bin-dir", "--server", "--weights"):
        parser.add_argument(flag, type=Path, required=True)
    args = parser.parse_args()
    with args.weights.open("rb") as f:
        assert hashlib.file_digest(f, "sha256").hexdigest() == PINS["model"]["sha256"]
    with tempfile.TemporaryDirectory(prefix="psios-local-model-") as tmp:
        work = Path(tmp)
        endpoint = work / "inference.sock"
        cmd = [args.server.resolve(), "-m", args.weights.resolve(), "--host", endpoint, "--alias", "psios-small",
               "-t", "2", "-c", "2048", "-np", "1", "-ngl", "0", "--no-webui"]
        with (work / "inference.log").open("w") as log, subprocess.Popen(cmd, stdout=log, stderr=log) as server:
            try:
                ready(server, endpoint)
                n = Node(args.bin_dir)
                try:
                    exercise(n, endpoint, work)
                finally:
                    n.close()
            finally:
                server.terminate()
                server.wait(timeout=15)
