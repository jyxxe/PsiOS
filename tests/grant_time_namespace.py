"""Reject a live grant in a different Linux time namespace on the same boot."""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import time

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
        node.stop()
        pidfile = node.base / "namespace-pid"
        wrapper = subprocess.Popen(["unshare", "--time", "--fork", "--kill-child=TERM", "--boottime", "30",
            "/bin/sh", "-ec", 'printf "%s\\n" "$$" > "$1"; shift; exec "$@"',
            "psios-time-fixture", pidfile, *node.daemon, "serve"])
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
        finally:
            if wrapper.poll() is None:
                if pid is not None:
                    try:
                        os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                else:
                    wrapper.terminate()
                wrapper.wait(20)
        node.start()
        assert status(original) == identity
        print("PASS authority survives a namespace restart; only matching clock evidence authorizes the old grant")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    exercise(parser.parse_args().bin_dir)
