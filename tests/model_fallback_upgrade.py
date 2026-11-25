"""A format-46 in-flight Provider decision still settles after installing model fallback."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key, target
from test_model_fallback import publish_skill, register_provider, submit_decision


def main(previous):
    node = Node(previous)
    try:
        provider, primary = register_provider(node, "before-fallback")
        skill = publish_skill(node)
        agent = node.call("Agent.Register", {"name": "migration-session-owner"})
        params = {
            "goal": "Keep the old decision", "model_ref": primary, "skill_ref": skill,
            "context_policy": "ReferencesOnly", "max_turns": 4, "provider_queue": True, "key": key(),
        }
        meter = node.call("Grant.Issue", {
            "principal_id": agent["principal_id"], "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "units": 2, "model_calls": 2, "ttl_seconds": 120,
        })
        params["delegation_id"] = meter["delegation_ref"]["resource_id"]
        session = node.call("Session.Open", params, agent["token"])["reference"]["resource_id"]
        claim_key = key()
        original = node.call("Model.ClaimNext", {"key": claim_key, "model_ref": primary}, provider["token"])
        assert original["session_id"] == session, original
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 46 and before["migration_required"], before
        node.start()
        same = node.call("Model.ClaimNext", {"key": claim_key, "model_ref": primary}, provider["token"])
        assert same["lease_id"] == original["lease_id"] and same["recovered"], same
        result = submit_decision(node, provider, original, {"action": "finish", "summary": "retained"})
        assert result["lifecycle"] == "Succeeded", result
        alternate, model = register_provider(node, "after-fallback")
        grant = node.call("Grant.Issue", {
            "principal_id": agent["principal_id"], "units": 2, "model_calls": 2, "ttl_seconds": 120,
            "actions": ["session.run", "task.submit", "task.read", "system.status", "session.failover"],
            "tool_targets": [primary["resource_id"], model["resource_id"], "profile-local"],
        })
        opened = node.call("Session.Open", {
            **params, "key": key(), "delegation_id": grant["delegation_ref"]["resource_id"],
            "fallback_model_refs": [model],
        }, agent["token"])["reference"]
        change = node.call("Session.UseFallback", {**target(opened), "model_ref": model}, agent["token"])
        next_turn = node.call("Model.ClaimNext", {"key": key(), "model_ref": model}, alternate["token"])
        assert change["successor"]["reference"]["resource_id"] == next_turn["session_id"]
        assert submit_decision(node, alternate, next_turn, {"action": "finish", "summary": "new provider"})["lifecycle"] == "Succeeded"
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 47 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 46 original model lease settled and new session handed off after migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
