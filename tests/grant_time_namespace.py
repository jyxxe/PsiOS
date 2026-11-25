"""Reject a live grant in a different Linux time namespace on the same boot."""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import time

from model_time_upgrade import open_lease
from support import Failure, Node


def exercise(binaries):
    node = Node(binaries)
    try:
        original_ns = os.readlink(f"/proc/{node.process.pid}/ns/time")
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text()
        agent = node.call("Agent.Register", {"name": "time-namespace"})
        request = {"principal_id": agent["principal_id"], "units": 1, "ttl_seconds": 180,
                   "actions": ["system.status"], "tool_targets": ["profile-local"]}

        def grant():
            return node.call("Grant.Issue", request)["delegation_ref"]["resource_id"]

        def status(delegation):
            return node.call("Tool.Call", {"tool": "system.status", "input": {}, "delegation_id": delegation},
                             agent["token"])["authority_id"]

        original = grant()
        identity = status(original)
        model_limit = 16_000
        agent_token, provider_token, budget, claim, lease = open_lease(node, model_limit)
        response = {"session_id": claim["session_id"], "lease_id": lease["lease_id"],
                    "decision": {"action": "finish", "summary": "original clock"}}
        assert node.call("Budget.Get", budget, agent_token)["model_lease_ms_charged"] == model_limit
        node.stop()
        pidfile = node.base / "namespace-pid"
        wrapper = subprocess.Popen(["unshare", "--time", "--fork", "--kill-child=TERM", "--boottime", "30",
            "/bin/sh", "-ec", 'printf "%s\\n" "$$" > "$1"; shift; exec "$@"',
            "psios-time-fixture", pidfile, *node.daemon, "serve"], start_new_session=True)
        pid = None
        try:
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                assert wrapper.poll() is None, "namespace daemon failed to start"
                try:
                    pid = int(pidfile.read_text())
                    if node.call("Principal.WhoAmI", {})["authority_id"] == identity:
                        break
                except (FileNotFoundError, ConnectionError, ValueError):
                    time.sleep(0.03)
            else:
                raise AssertionError("namespace daemon never became ready")
            assert Path(f"/proc/{pid}/root/proc/sys/kernel/random/boot_id").read_text() == boot_id
            assert os.readlink(f"/proc/{pid}/ns/time") != original_ns
            try:
                status(original)
            except Failure as error:
                assert error.code == "PermissionDenied", error
            else:
                raise AssertionError("old grant was used under a different clock")
            assert status(grant()) == identity
            try:
                node.call("Model.Respond", response, provider_token)
            except Failure as error:
                assert error.code in ("PermissionDenied", "Fenced"), error
            else:
                raise AssertionError("metered lease accepted time from another namespace")
            assert node.call("Budget.Get", budget, agent_token)["model_lease_ms_charged"] == model_limit
        finally:
            if wrapper.poll() is None:
                if pid is not None:
                    try:
                        os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                else:
                    wrapper.terminate()
                try:
                    wrapper.wait(20)
                except subprocess.TimeoutExpired:
                    os.killpg(wrapper.pid, signal.SIGKILL)
                    wrapper.wait(5)
                    raise
        node.start()
        assert status(original) == identity
        receipt = node.call("Model.Respond", response, provider_token)
        usage = node.call("Resource.Resolve", {"reference": receipt["usage_ref"]})["value"]
        elapsed = usage["authority_elapsed_lease_ms"]
        assert 1 <= elapsed < model_limit
        assert node.call("Budget.Get", budget, agent_token)["model_lease_ms_charged"] == elapsed
        print("PASS authority preserves grants and settles model time only under the original clock")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    exercise(parser.parse_args().bin_dir)
