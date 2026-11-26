"""Use real pre-brokered Core state to check the input-custody format cutover."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import unittest

from support import key
from test_agent_session_inputs import SessionInputFixture


class BrokeredSessionInputUpgrade(SessionInputFixture, unittest.TestCase):
    def test_previous_queued_input_and_new_task_custody(self):
        n = self.n
        token = self.agent["token"]
        grant = self.grant(self.agent, [
            "agent.session.start", "session.run", "session.context.read", "task.submit", "task.read",
            "node.inventory", "artifact.read",
        ], [self.node_id, self.model["resource_id"], self.skill["resource_id"], self.artifact["resource_id"]])
        opening = self.opening(grant, provider_queue=True)
        blueprint = {field: opening[field] for field in [
            "goal", "skill_ref", "model_ref", "context_policy", "max_turns", "input_artifacts",
        ]}
        delegation = grant["delegation_ref"]["resource_id"]
        launch = {"tool": "agent.session.start", "input": blueprint, "key": key(), "delegation_id": delegation}
        self.refuse("InvalidArgument", "Task.Prepare", launch, token)
        existing = self.opened(opening)
        prior = self.queued("before-launch-custody", accept=True)
        self.assertEqual(prior["session_id"], existing["resource_id"])
        self.assertEqual(prior["context"]["inputs"][0]["value"], self.data)

        old_daemon = n.daemon[:]
        n.stop(signal.SIGKILL)
        previous = json.loads(subprocess.check_output(old_daemon + ["verify"], timeout=20))
        self.assertIn(previous["database_format"], (104, 105))
        n.daemon[0] = str(Path(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug")).absolute() / "psiosd")
        staged = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertTrue(staged["migration_required"])
        n.start()
        self.assertEqual(self.queued("before-launch-custody", accept=True), {**prior, "recovered": True})
        self.respond(existing, prior, {"action": "finish", "summary": "Original session completed."})

        request = n.call("Task.Prepare", launch, token)
        accepted = n.call("Task.Submit", request, token)
        task = n.wait(accepted["task_id"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"))
        receipt = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)
        attached = receipt["session_ref"]
        self.assertNotEqual(existing["resource_id"], attached["resource_id"])
        self.assertEqual(self.session(attached)["session"]["origin_task_ref"]["resource_id"], task["task_id"])
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), token)
        n.stop(signal.SIGKILL)
        refused = subprocess.run(old_daemon + ["serve"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(refused.returncode, 0, refused)
        self.assertIn("SchemaVersionUnsupported", refused.stderr)
        migrated = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertGreaterEqual(migrated["database_format"], 106)
        self.assertEqual(migrated["node_id"], previous["node_id"])
        self.assertFalse(migrated["migration_required"])

        n.start()
        self.assertEqual(n.call("Task.Submit", request, token), {**accepted, "deduplicated": True})
        self.assertEqual(self.queued("after-launch-custody-legacy"), {"available": False})
        current = self.queued("after-launch-custody", accept=True)
        self.assertEqual(current["session_id"], attached["resource_id"])
        self.assertEqual(current["context"]["inputs"], prior["context"]["inputs"])
        self.respond(attached, current, {"action": "finish", "summary": "Brokered session completed."})
        n.call("Artifact.Withdraw", self.withdrawal(), token)
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin-dir", required=True)
    BrokeredSessionInputUpgrade.bin_dir = parser.parse_args().previous_bin_dir
    unittest.main(argv=[__file__])
