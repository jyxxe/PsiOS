"""Record late evidence against a strict Unknown issued by a real format-94 Core."""
import argparse
from pathlib import Path
import signal

from project_upgrade import verify
from service_journal import refuses
from support import Node, key, target
from test_model_fallback import publish_skill
from test_model_late_decision import OBSERVE, STATUS, digest


def upgrade(previous, current):
    node = Node(previous)
    try:
        provider = node.call("Agent.Register", {"name": "pre-upgrade-model-provider"})
        owner = node.call("Agent.Register", {"name": "pre-upgrade-session-owner"})
        model = node.call("Model.Register", {
            "name": "format-94-strict", "revision": key(),
            "provider_principal_id": provider["principal_id"],
            "inference_outcome_policy": "Strict", "lease_seconds": 30,
            "max_context_bytes": 16384, "max_output_bytes": 4096,
        })
        node.call("Grant.Issue", {
            "principal_id": provider["principal_id"], "actions": ["model.serve"],
            "tool_targets": [model["resource_id"]], "units": 4, "ttl_seconds": 180,
        })
        skill = publish_skill(node)
        grant = node.call("Grant.Issue", {
            "principal_id": owner["principal_id"],
            "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "tool_targets": [model["resource_id"], skill["resource_id"], "profile-local"],
            "units": 4, "model_calls": 4, "model_lease_ms": 120000, "ttl_seconds": 180,
        })
        session = node.call("Session.Open", {
            "key": key(), "goal": "Keep witnessed inference across a format upgrade",
            "model_ref": model, "skill_ref": skill, "context_policy": "ReferencesOnly", "max_turns": 2,
            "delegation_id": grant["delegation_ref"]["resource_id"],
        }, owner["token"])["reference"]["resource_id"]
        claim = node.call("Model.Claim", {"session_id": session, "key": key()}, provider["token"])
        assert claim["inference_outcome_policy"] == "Strict" and "late_decision_observation" not in claim
        issued = node.call("Session.Get", {"id": session}, owner["token"])["reference"]
        lease = claim["lease_id"]
        unknown = node.call("Model.ReportFailure", {
            "session_id": session, "lease_id": lease, "model_ref": model, "outcome": "Unknown",
        }, provider["token"])
        request = {"session_id": session, "lease_id": lease, "decision": STATUS}
        refuses("UnsupportedCapability", lambda: node.call(OBSERVE, request, provider["token"]))
        previous_history = node.call("Session.History", {"id": session}, owner["token"])
        balance = node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, owner["token"])
        node.stop(signal.SIGKILL)
        before = verify(node)
        assert before["database_format"] == 94 and not before["migration_required"], before
        head = (node.witness / "head").read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        pending = verify(node)
        assert pending["database_format"] == 94 and pending["migration_required"], pending
        assert (node.witness / "head").read_bytes() == head

        node.start()
        assert node.call("Session.History", {"id": session}, owner["token"]) == previous_history
        recorded = node.call(OBSERVE, request, provider["token"])
        assert recorded["status"] == "Recorded", recorded
        evidence = recorded["observation"]
        assert evidence["issued_session_ref"] == issued and evidence["unknown_session_ref"] == unknown["session_ref"]
        assert evidence["lease_id"] == lease and evidence["decision_digest"] == digest(STATUS)
        assert node.call(OBSERVE, request, provider["token"]) == recorded
        history = node.call("Session.History", {"id": session}, owner["token"])
        assert history["session_ref"] == unknown["session_ref"] and not history["tasks"], history
        assert history["turns"] == [{**previous_history["turns"][0], "late_decision_observation": evidence}]
        assert node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, owner["token"]) == balance
        refuses("PreconditionFailed", lambda: node.call("Session.Resume", target(unknown["session_ref"]), owner["token"]))
        node.stop()
        after = verify(node)
        assert after["database_format"] >= 95 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"] and after["witness"] == "Confirmed"
        print("PASS original format-94 strict Unknown and authority admit digest-only evidence after migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
