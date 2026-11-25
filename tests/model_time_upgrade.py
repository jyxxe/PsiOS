"""Upgrade a format 36 authority with a live unmetered model lease."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key


def open_lease(node, time_limit=None):
    agent = node.call("Agent.Register", {"name": "lease-agent"})
    provider, _ = node.agent(["model.serve"])
    request = {"principal_id": agent["principal_id"], "units": 2, "model_calls": 3, "ttl_seconds": 120,
               "actions": ["session.run", "task.submit", "task.read", "node.inventory"]}
    if time_limit is not None:
        request["model_lease_ms"] = time_limit
    grant = node.call("Grant.Issue", request)
    model = node.call("Model.Register", {
        "name": "lease-model", "revision": "original", "provider_principal_id": provider["principal_id"],
        "lease_seconds": 20, "max_context_bytes": 131072, "max_output_bytes": 8192,
    })
    skill = node.call("Skill.Publish", {
        "name": "lease-scope", "version": "1", "source": "lease-test", "license": "0BSD",
        "instructions": "Collect local inventory.", "tools": ["node.inventory"],
    })
    session = node.call("Session.Open", {
        "key": key(), "goal": "Inventory", "model_ref": model, "skill_ref": skill,
        "delegation_id": grant["delegation_ref"]["resource_id"], "context_policy": "ReferencesOnly", "max_turns": 2,
    }, agent["token"])["reference"]
    claim = {"session_id": session["resource_id"], "key": "persistent-model-lease"}
    lease = node.call("Model.Claim", claim, provider["token"])
    return agent["token"], provider["token"], {"id": grant["budget_ref"]["resource_id"]}, claim, lease


def main(previous):
    node = Node(previous)
    try:
        agent_token, provider_token, params, claim, lease = open_lease(node)
        budget = node.call("Budget.Get", params, agent_token)
        assert budget["model_calls_used"] == 1 and "model_lease_ms_maximum" not in budget, budget
        node.stop(signal.SIGKILL)

        node.daemon[0] = str(Path("target/debug/psiosd").absolute())
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert before["database_format"] == 36 and before["migration_required"], before
        node.start()
        assert node.call("Budget.Get", params, agent_token) == budget
        replayed = node.call("Model.Claim", claim, provider_token)
        assert replayed["lease_id"] == lease["lease_id"] and replayed["expires_at"] == lease["expires_at"], replayed
        receipt = node.call("Model.Respond", {
            "session_id": claim["session_id"], "lease_id": lease["lease_id"],
            "decision": {"action": "finish", "summary": "completed after upgrade"},
        }, provider_token)
        assert receipt["lifecycle"] == "Succeeded" and "usage_ref" not in receipt, receipt
        assert node.call("Budget.Get", params, agent_token) == budget
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 37 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"]
        print("PASS format 36 retained its live unmetered model lease and original call charge")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
