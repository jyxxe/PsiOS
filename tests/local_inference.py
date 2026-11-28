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

from model_delivery import interrupt_delivery
from support import Node, key, target
from test_pci_observation import linux_devices

PINS = json.loads((Path(__file__).resolve().parent / "fixtures/local-inference.json").read_text())
GENERATION_TOKENS = 192
DEVICE_NAMES = {"node.pci": "PCI", "node.usb": "USB"}


def linux_usb_devices():
    bus = Path("/sys/bus/usb/devices")
    assert bus.is_dir(), "USB observation requires Linux USB sysfs"
    rows = {}
    for device in bus.iterdir():
        if not (device / "idVendor").exists():
            continue
        rows[device.name] = {
            "vendor_id": (device / "idVendor").read_text().strip(),
            "product_id": (device / "idProduct").read_text().strip(),
            "device_address": int((device / "devnum").read_text()),
            "authorized": (device / "authorized").read_text().strip() == "1",
        }
    return rows


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


def exercise(node, endpoint, work, *, metered, tool):
    provider = node.call("Agent.Register", {"name": "local-cpu-inference"})
    agent = node.call("Agent.Register", {"name": "local-inventory"})
    node.call("Grant.Issue", dict(principal_id=provider["principal_id"], actions=["model.serve"], units=1, ttl_seconds=300))
    model = node.call("Model.Register", dict(name="SmolLM2-135M-Instruct", revision="Q4_K_M",
                      provider_principal_id=provider["principal_id"], lease_seconds=60, max_context_bytes=16384,
                      max_output_bytes=4096, max_in_flight=8))
    goal = f"Call {tool} now with an empty input object."
    instructions = f"Call {tool} with an empty input to obtain the local hardware inventory."
    if tool in DEVICE_NAMES:
        device_name = DEVICE_NAMES[tool]
        goal = f"Inspect local {device_name} devices before finishing."
        if tool == "node.pci":
            goal = "Inspect local PCI devices and their Linux drivers before finishing."
        example = json.dumps({"action": "tool", "tool": tool, "input": {}}, separators=(",", ":"))
        instructions = f"Read the Linux {device_name} inventory first. Your first decision must be {example}"
    skill = node.call("Skill.Publish", dict(name="read-local-inventory", version="1", tools=[tool],
                     instructions=instructions,
                     license="MIT", source="operator"))
    scope = {}
    if tool in DEVICE_NAMES:
        local = node.call("Node.Get", {})["reference"]["resource_id"]
        scope["tool_targets"] = [local, model["resource_id"], skill["resource_id"]]
    grant = node.call("Grant.Issue", dict(principal_id=agent["principal_id"], units=8, model_calls=8,
                      actions=["session.run", "task.submit", "task.read", tool], ttl_seconds=300, **scope))
    config = dict(socket=str(endpoint), server_uid=os.geteuid(), model_ref=model, model_alias="psios-small",
                  max_tokens=GENERATION_TOKENS, timeout_ms=30000)
    cfg = work / "local-model.json"
    credential = work / "provider.token"
    credential.write_text(provider["token"])
    credential.chmod(0o600)
    journal = work / "journal"
    journal.mkdir(mode=0o700)

    def start(queued=False):
        result = node.call("Session.Open", dict(key=key(), goal=goal,
                           skill_ref=skill, model_ref=model, delegation_id=grant["delegation_ref"]["resource_id"],
                           context_policy="ReferencesOnly", max_turns=2, **({"provider_queue": True} if queued else {})),
                           agent["token"])
        return result["reference"]["resource_id"]

    def step(sid, settings):
        cfg.write_text(json.dumps(settings))
        return subprocess.run([node.binaries / "psiosctl", "--socket", node.socket, "--token-file", credential,
                               "model-step", sid, "--key", "first", "--decision-dir", journal, "--local-model", cfg],
                              capture_output=True, text=True, timeout=45)

    def check_observation(task_id):
        task = node.wait(task_id)
        assert task["lifecycle"] == "Succeeded", task
        run = node.call("Run.Get", {"id": task["current_run_id"]})
        snapshot = node.call("Resource.Resolve", {"reference": run["execution_snapshot_ref"]})["value"]
        assert snapshot["tool_refs"][0]["resource_id"] == "tool-" + tool and snapshot["model_ref"] == model
        output = node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, agent["token"])
        assert output["boot_id"] == Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        if tool == "node.pci":
            assert output["devices"] == linux_devices(), output
        elif tool == "node.usb":
            expected = linux_usb_devices()
            devices = output["devices"]
            assert [row["location"] for row in devices] == sorted(expected)[:len(devices)], output
            for row in devices:
                assert all(row[key] == value for key, value in expected[row["location"]].items()), output
            assert bool(output["next_cursor"]) == (len(devices) < len(expected)), output
        else:
            assert output["memory_bytes"]["MemTotal"] > 0

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
                            (dict(config, max_tokens=8), "local model response exhausted its output limit")]:
        sid = start()
        result = step(sid, settings)
        assert result.returncode != 0 and error in result.stderr, (error, result.stderr)
        assert not node.call("Session.History", {"id": sid}, agent["token"])["tasks"]
        current = node.call("Session.Get", {"id": sid}, agent["token"])
        node.call("Session.Cancel", target(current["reference"]), agent["token"])
    sid = start()
    cfg.write_text(json.dumps(config))
    with interrupt_delivery(node, credential, sid, "first", journal, committed=False, local_model=cfg):
        pending = json.loads((journal / "psios-model-pending.json").read_bytes())["record"]
        assert pending["format"] == (2 if metered else 1)
        retained_usage = pending["request"].get("usage")
        assert (retained_usage is not None) == metered
    result = step(sid, dict(config, socket=str(work / "absent.sock")))
    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)
    assert "task_id" in receipt, receipt
    check_observation(receipt["task_id"])
    if metered:
        usage = node.call("Resource.Resolve", {"reference": receipt["usage_ref"]})["value"]
        assert usage["reported_tokens"] == retained_usage
        assert usage["measurement_source"] == "ProviderReported"
        assert usage["provider_principal_id"] == provider["principal_id"] and usage["budget_ref"] == grant["budget_ref"]
        tokens = usage["reported_tokens"]
        assert tokens["input_tokens"] > 0 and 0 < tokens["output_tokens"] <= GENERATION_TOKENS
        print(f"Recorded provider-reported input and output tokens {usage['reported_tokens']} on the original reservation.")
    else:
        assert "usage_ref" not in receipt
    # A saved receipt can be queried without contacting an inference service again.
    replay = step(sid, dict(config, socket=str(work / "absent.sock")))
    assert replay.returncode == 0 and json.loads(replay.stdout) == receipt
    assert not list(journal.iterdir())
    history = node.call("Session.History", {"id": sid}, agent["token"])
    assert len(history["turns"]) == len(history["tasks"]) == 1
    budget = node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, agent["token"])
    assert (budget["model_calls_used"], budget["spent"]) == (6, 1), budget
    print("Verified real SmolLM2 CPU inference, UID and model binding, offline failure, one authorized"
          f" {tool} Task {receipt['task_id']} and deduplicated receipt replay.")

    if metered:
        queued = start(queued=True)
        reference = work / "model-ref.json"
        reference.write_text(json.dumps(model))
        cfg.write_text(json.dumps(config))
        dispatched = subprocess.run([node.binaries / "psiosctl", "--socket", node.socket,
            "--token-file", credential, "model-worker", reference, "--once", "--wait-ms", "2000",
            "--decision-dir", journal, "--local-model", cfg], capture_output=True, text=True, timeout=45)
        assert dispatched.returncode == 0, dispatched.stderr
        receipt = json.loads(dispatched.stdout)
        assert receipt["session_ref"]["resource_id"] == queued, receipt
        check_observation(receipt["task_id"])
        usage = node.call("Resource.Resolve", {"reference": receipt["usage_ref"]})["value"]
        tokens = usage["reported_tokens"]
        assert tokens["input_tokens"] > 0 and 0 < tokens["output_tokens"] <= GENERATION_TOKENS
        budget = node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, agent["token"])
        assert (budget["model_calls_used"], budget["spent"]) == (7, 2), budget
        print(f"Verified autonomous provider discovery and real CPU inference for {tool} Session {queued}.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--bin-dir", "--server", "--weights"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--tool", choices=("node.inventory", *DEVICE_NAMES), default="node.inventory")
    parser.add_argument("--legacy-unmetered", action="store_true", help="expect a pre-metering authority with the current CLI")
    args = parser.parse_args()
    with args.weights.open("rb") as f:
        assert hashlib.file_digest(f, "sha256").hexdigest() == PINS["model"]["sha256"]
    with tempfile.TemporaryDirectory(prefix="psios-local-model-") as tmp:
        work = Path(tmp)
        endpoint = work / "inference.sock"
        cmd = [args.server.resolve(), "-m", args.weights.resolve(), "--host", endpoint, "--alias", "psios-small",
               "-t", "2", "-c", "2048", "-np", "1", "-ngl", "0", "--no-webui",
               "--seed", "1", "--no-cache-prompt"]
        with (work / "inference.log").open("w") as log, subprocess.Popen(cmd, stdout=log, stderr=log) as server:
            try:
                ready(server, endpoint)
                n = Node(args.bin_dir)
                try:
                    exercise(n, endpoint, work, metered=not args.legacy_unmetered, tool=args.tool)
                finally:
                    n.close()
            finally:
                server.terminate()
                server.wait(timeout=15)
