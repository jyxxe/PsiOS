"""Preserve a live legacy model lease while adding opt-in bounded context."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key


def verify(node):
    return json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))


def upgrade(previous, current, previous_format):
    node = Node(previous)
    try:
        agent = node.call("Agent.Register", {"name": "upgrade-context-consumer"})
        provider, _ = node.agent(["model.serve"])
        model = node.call("Model.Register", {
            "name": "upgrade-context-provider", "revision": "1", "provider_principal_id": provider["principal_id"],
            "max_context_bytes": 16384, "max_output_bytes": 1024, "lease_seconds": 60,
        })
        skill = node.call("Skill.Publish", {"name": "legacy-observations", "version": "1", "source": "local-test",
            "license": "0BSD", "instructions": "Summarize the node status.", "tools": ["system.status"]})
        # Model calls are propagated through the session's root delegation.
        grant = node.call("Grant.Issue", {"principal_id": agent["principal_id"],
            "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "units": 5, "model_calls": 5, "ttl_seconds": 180})
        params = {"key": key(), "goal": "Observe this node", "model_ref": model, "skill_ref": skill,
                  "delegation_id": grant["delegation_ref"]["resource_id"], "context_policy": "LocalToolResults",
                  "max_turns": 3, "provider_queue": True}
        session = node.call("Session.Open", params, agent["token"])["reference"]["resource_id"]

        def claim(name):
            return node.call("Model.ClaimNext", {"model_ref": model, "key": name}, provider["token"])

        def respond(turn, decision):
            return node.call("Model.Respond", {"session_id": turn["session_id"], "lease_id": turn["lease_id"],
                                  "decision": decision}, provider["token"])

        task = respond(claim("old-tool"), {"action": "tool", "tool": "system.status", "input": {}})["task_id"]
        assert node.wait(task)["lifecycle"] == "Succeeded"
        old = claim("old-inflight")
        assert old["session_id"] == session and old["context"]["last_tool"]["task_id"] == task
        assert "context_digest" not in old
        node.stop(signal.SIGKILL)

        node.daemon[0] = str((current / "psiosd").absolute())
        prior = verify(node)
        assert prior["database_format"] == previous_format and prior["migration_required"], prior
        node.start()
        assert claim("old-inflight") == {**old, "recovered": True}
        reply = respond(old, {"action": "finish", "summary": "old context settled exactly once"})
        assert reply["lifecycle"] == "Succeeded" and claim("old-inflight")["completed"] == reply
        admitted = node.call("Session.Open", {**params, "key": key(), "tool_context_window": 2},
                             agent["token"])["reference"]
        new = claim("new-context")
        assert new["session_id"] == admitted["resource_id"] and new["context_digest"].startswith("sha256:")
        assert new["context"]["previous_tools"] == [] and new["context"]["last_tool"] is None
        node.stop()
        migrated = verify(node)
        assert migrated["database_format"] > previous_format and not migrated["migration_required"], migrated
        assert migrated["node_id"] == prior["node_id"]
        print(f"PASS format {previous_format} active lease, exact original context, and newly enabled bounded context")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    parser.add_argument("--previous-format", type=int, default=64)
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin, args.previous_format)
