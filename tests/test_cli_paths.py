"""The client follows the daemon's endpoint configuration and keeps explicit overrides."""
import json
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

from support import Node


class CliPaths(unittest.TestCase):
    def test_config_and_explicit_credentials(self):
        n = Node("target/debug")
        self.addCleanup(n.close)
        client = [n.binaries / "psiosctl", "--config", n.config]
        owner = json.loads(subprocess.check_output(client + ["whoami"]))
        actor, _ = n.agent(["task.read"])
        token = n.base / "agent.token"
        token.touch(mode=0o600)
        token.write_text(actor["token"])
        other = json.loads(subprocess.check_output(client + ["--token-file", token, "whoami"]))
        self.assertNotEqual(other["principal_id"], owner["principal_id"])
        self.assertEqual(other["principal_id"], actor["principal_id"])
        invalid = subprocess.run(client + ["--socket", n.base / "absent.sock", "whoami"], capture_output=True)
        self.assertNotEqual(invalid.returncode, 0)
        conflict = subprocess.run(client + ["--state", n.state, "whoami"], capture_output=True)
        self.assertNotEqual(conflict.returncode, 0)

    def test_explicit_local_state_layout(self):
        binaries = Path("target/debug").resolve()
        with tempfile.TemporaryDirectory() as temp:
            state = ["--state", str(Path(temp) / "local")]
            daemon = [binaries / "psiosd", *state]
            client = [binaries / "psiosctl", *state]
            subprocess.run(daemon + ["init"], check=True, capture_output=True)
            with subprocess.Popen(daemon + ["serve"]) as process:
                try:
                    for _ in range(100):
                        if (Path(state[1]) / "api.sock").exists():
                            break
                        time.sleep(0.025)
                    result = json.loads(subprocess.check_output(client + ["node"]))
                    self.assertEqual(result["state"]["lifecycle"], "Ready")
                finally:
                    process.terminate()
                    self.assertEqual(process.wait(timeout=10), 0)
