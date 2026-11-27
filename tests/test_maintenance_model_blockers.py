"""Inspect genuine current, residual and unresolved strict model leases."""
import os
from pathlib import Path
import signal
import time
import unittest

from project_upgrade import verify
from support import Node, key
from test_model_fallback import publish_skill


class MaintenanceModelBlockersTests(unittest.TestCase):
    def test_persisted_unknown_outlives_the_original_provider_slot(self):
        binaries = Path(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")).resolve()
        previous = os.getenv("PSIOS_TEST_MODEL_BLOCKERS_PREVIOUS")
        node = Node(previous or binaries)
        self.addCleanup(node.close)
        provider, _ = node.agent(["model.serve"])
        model = node.call("Model.Register", {
            "name": "maintenance-model", "revision": key(),
            "provider_principal_id": provider["principal_id"],
            "lease_seconds": 6, "max_in_flight": 2,
            "max_context_bytes": 16384, "max_output_bytes": 8192,
            "inference_outcome_policy": "Strict",
        })
        skill = publish_skill(node)
        agent = node.call("Agent.Register", {"name": "maintenance-session-owner"})
        grant = node.call("Grant.Issue", {
            "principal_id": agent["principal_id"],
            "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "tool_targets": [model["resource_id"], skill["resource_id"], "profile-local"],
            "units": 8, "model_calls": 6, "model_lease_ms": 36000, "ttl_seconds": 120,
        })

        def session(queued):
            result = node.call("Session.Open", {
                "key": key(), "goal": "Inspect model effects during maintenance",
                "provider_queue": queued, "skill_ref": skill, "model_ref": model,
                "context_policy": "ReferencesOnly", "max_turns": 4,
                "delegation_id": grant["delegation_ref"]["resource_id"],
            }, agent["token"])
            return result["reference"]["resource_id"]

        direct_id, queued_id = session(False), session(True)
        direct = node.call("Model.Claim", {"session_id": direct_id, "key": key()}, provider["token"])
        queued = node.call("Model.ClaimNext", {"model_ref": model, "key": key()}, provider["token"])
        self.assertEqual(queued["session_id"], queued_id)
        admission = node.call("Node.WorkAdmission.Get", {})["reference"]
        draining = node.call("Node.WorkAdmission.Set", {
            "expected_resource_version": admission["version"], "mode": "Draining",
            "reason": "identify ongoing provider effects before maintenance",
        })["reference"]

        if previous:
            node.stop(signal.SIGKILL)
            old = verify(node)
            self.assertEqual(old["database_format"], 93)
            self.assertFalse(old["migration_required"])
            witness = (node.witness / "head").read_bytes()
            node.binaries = binaries
            node.daemon[0] = str(binaries / "psiosd")
            before = verify(node)
            self.assertEqual(before["database_format"], 93)
            self.assertTrue(before["migration_required"])
            self.assertEqual((node.witness / "head").read_bytes(), witness)
            node.start()
            self.assertEqual(node.call("Node.WorkAdmission.Get", {})["reference"], draining)

        def observe(limit=8):
            snapshot = node.call("Node.WorkAdmission.Blockers", {
                "expected_resource_version": draining["version"], "limit": limit,
            })
            self.assertEqual(snapshot["reference"], draining)
            return snapshot

        short = observe(1)
        self.assertTrue(short["model_leases"]["truncated"])
        self.assertEqual(len(short["model_leases"]["items"]), 1)
        self.assertEqual(short["model_outcomes_unknown"], {"items": [], "truncated": False})
        current = observe()["model_leases"]
        self.assertFalse(current["truncated"])
        current = {item["lease_id"]: item for item in current["items"]}
        self.assertEqual(set(current), {direct["lease_id"], queued["lease_id"]})
        for session_id, claim in ((direct_id, direct), (queued_id, queued)):
            item = current[claim["lease_id"]]
            self.assertEqual(item["session_ref"], node.get(session_id)["reference"])
            self.assertEqual(item["model_ref"], model)
            self.assertEqual((item["state"], item["expires_at"]), ("Current", claim["expires_at"]))

        unknown = node.call("Model.ReportFailure", {
            "session_id": direct_id, "model_ref": model,
            "lease_id": direct["lease_id"], "outcome": "Unknown",
        }, provider["token"])
        done = node.call("Model.Respond", {
            "session_id": queued_id, "lease_id": queued["lease_id"],
            "decision": {"action": "finish", "summary": "Settled before maintenance."},
        }, provider["token"])
        self.assertEqual(done["lifecycle"], "Succeeded")
        snapshot = observe()
        self.assertEqual(snapshot["model_leases"]["items"], [{
            **current[direct["lease_id"]], "session_ref": unknown["session_ref"], "state": "Residual",
        }])
        expected_outcome = {
            "items": [{
                "session_ref": unknown["session_ref"], "model_ref": model,
                "lease_id": direct["lease_id"], "source": "Provider",
            }],
            "truncated": False,
        }
        self.assertEqual(snapshot["model_outcomes_unknown"], expected_outcome)
        node.stop(signal.SIGKILL)
        verified = verify(node)
        self.assertGreaterEqual(verified["database_format"], 94)
        self.assertEqual(verified["sqlite_integrity"], "Verified")
        node.start()
        deadline = time.monotonic() + 10
        while (snapshot := observe())["model_leases"]["items"]:
            self.assertLess(time.monotonic(), deadline, snapshot)
            time.sleep(0.05)
        self.assertEqual(snapshot["model_outcomes_unknown"], expected_outcome)
        observed = node.call("Node.WorkAdmission.WaitQuiescent", {
            "expected_resource_version": draining["version"], "timeout_ms": 100,
        })
        self.assertIs(observed["quiescent"], False, observed)
        self.assertEqual(observed["blockers"]["model_leases"], {"items": [], "truncated": False})
        self.assertEqual(observed["blockers"]["model_outcomes_unknown"], expected_outcome)


if __name__ == "__main__":
    unittest.main()
