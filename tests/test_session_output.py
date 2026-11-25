"""Publish real model lease output through the local authority and retain its provenance."""
import base64
import hashlib
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target
from test_artifact_install import grant
from test_model_fallback import publish_skill, register_provider, submit_decision


TOOL = "artifact.publish_session_summary"


def completed_session(node, principal, token, summary):
    provider, model = register_provider(node, "artifact-output-provider")
    skill = publish_skill(node)
    capacity = node.call("Grant.Issue", {"principal_id": principal, "actions": [
        "session.run", "task.submit", "task.read", "system.status"],
        "units": 4, "model_calls": 4, "ttl_seconds": 180})
    session = node.call("Session.Open", {"goal": "Prepare the final output",
        "provider_queue": True, "max_turns": 3, "context_policy": "LocalToolResults",
        "skill_ref": skill, "model_ref": model, "key": key(),
        "delegation_id": capacity["delegation_ref"]["resource_id"]}, token)["reference"]
    first = node.call("Model.ClaimNext", {"model_ref": model, "key": key()}, provider["token"])
    assert first["session_id"] == session["resource_id"], first
    observed = submit_decision(node, provider, first, {"action": "tool", "tool": "system.status", "input": {}})
    assert node.wait(observed["task_id"])["lifecycle"] == "Succeeded"
    final = node.call("Model.ClaimNext", {"model_ref": model, "key": key()}, provider["token"])
    assert final["turn"] == 2 and final["session_id"] == session["resource_id"], final
    accepted = submit_decision(node, provider, final, {"action": "finish", "summary": summary})
    assert accepted["lifecycle"] == "Succeeded", accepted
    return provider, final, accepted, session


def prepare_publication(node, actor_token, session_ref, permitted, media, submission_key=None):
    return node.prepare(TOOL, {"session_ref": session_ref, "media_type": media},
                        submission_key, actor_token, permitted["delegation_ref"]["resource_id"])


def finish_publication(node, actor_token, request):
    admitted = node.call("Task.Submit", request, actor_token)
    scope = node.wait(admitted["task_id"])
    assert scope["lifecycle"] == "Succeeded", scope
    reference, = scope["result"]["artifact_refs"]
    return node.call("Artifact.Read", {"reference": reference}, actor_token)


def generate_artifact(node, principal, token, text, media):
    _, _, terminal, _ = completed_session(node, principal, token, text)
    session = terminal["session_ref"]
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    grant(node, principal, ["artifact.upload"], [node_id])
    permitted = grant(node, principal, ["task.submit", "task.read", TOOL], [node_id, session["resource_id"]])
    result = finish_publication(node, token, prepare_publication(node, token, session, permitted, media))
    assert result["decision_digest"] == terminal["decision_digest"]
    return result["artifact_ref"]


class SessionOutputTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.owner = self.node.call("Agent.Register", {"name": "artifact-author"})
        self.node_id = self.node.call("Node.Get", {})["reference"]["resource_id"]
        self.summary = "节点检查完成\nSummary ✓\n"
        self.provider, self.lease, self.terminal, self.initial = completed_session(
            self.node, self.owner["principal_id"], self.owner["token"], self.summary)
        self.session = self.terminal["session_ref"]
        self.scope = [self.node_id, self.session["resource_id"]]

    def denied(self, code, method, params, token):
        with self.assertRaises(Failure) as refused:
            self.node.call(method, params, token)
        self.assertEqual(refused.exception.code, code, refused.exception)

    def test_authentic_output_survives_restart_and_requires_separate_authority(self):
        n = self.node
        principal, token = self.owner["principal_id"], self.owner["token"]
        actions = ["task.submit", "task.read", TOOL]
        allowed = grant(n, principal, actions, self.scope)
        request = prepare_publication(n, token, self.session, allowed, "text/plain")
        self.denied("PermissionDenied", "Task.Submit", request, token)
        grant(n, principal, ["artifact.upload"], [self.node_id])
        insufficient = grant(n, principal, actions, [self.node_id])
        self.denied("PermissionDenied", "Task.Submit",
                    prepare_publication(n, token, self.session, insufficient, "text/plain"), token)
        historic = prepare_publication(n, token, self.initial, allowed, "text/plain")
        self.denied("PreconditionFailed", "Task.Submit", historic, token)

        foreign = self.provider["principal_id"]
        grant(n, foreign, ["artifact.upload"], [self.node_id])
        external = grant(n, foreign, actions, self.scope)
        thief = prepare_publication(n, self.provider["token"], self.session, external, "text/plain")
        self.denied("PermissionDenied", "Task.Submit", thief, self.provider["token"])
        n.call("Principal.Revoke", target(n.get(foreign)["reference"]))

        result = finish_publication(n, token, request)
        artifact = result["artifact_ref"]
        self.assertEqual(artifact["digest"], "sha256:" + hashlib.sha256(self.summary.encode()).hexdigest())
        self.denied("PermissionDenied", "Artifact.Get", {"reference": artifact}, token)
        grant(n, principal, ["artifact.read", "artifact.withdraw"], [self.node_id])
        data = n.call("Artifact.ReadChunk", {"reference": artifact, "offset": 0, "length": 100}, token)
        self.assertEqual(base64.b64decode(data["data_base64"]), self.summary.encode())
        published = n.call("Artifact.Get", {"reference": artifact}, token)["resource"]["spec"]
        self.assertEqual(published["declared_media_type"], "text/plain")
        proof = published["source"]
        self.assertEqual(proof["kind"], "LocalSessionOutput")
        self.assertEqual(proof["principal_id"], principal)
        self.assertEqual(proof["provider_principal_id"], foreign)
        self.assertEqual(proof["session_ref"], self.session)
        self.assertEqual(proof["model_lease_id"], self.lease["lease_id"])
        self.assertEqual(proof["decision_digest"], self.terminal["decision_digest"])
        self.assertEqual(proof["publication_operation_id"], result["operation_id"])

        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(finish_publication(n, token, request), result)
        self.assertTrue(n.call("Task.Submit", request, token)["deduplicated"])
        n.call("Artifact.Withdraw", {"reference": artifact, "reason": "retain only the session summary"}, token)
        self.assertEqual(n.call("Artifact.Get", {"reference": artifact}, token)["availability"], "Evicted")
        session = n.call("Session.Get", {"id": self.session["resource_id"]}, token)
        self.assertEqual(session["session"]["summary"], self.summary)
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, timeout=15, stdout=subprocess.DEVNULL)

    def test_failed_publication_rolls_back_ingress_and_verified_provenance_rejects_tampering(self):
        n = self.node
        principal, token = self.owner["principal_id"], self.owner["token"]
        grant(n, principal, ["artifact.upload"], [self.node_id])
        allowed = grant(n, principal, ["task.submit", "task.read", TOOL], self.scope)
        n.stop()
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("""CREATE TRIGGER withhold_model_artifact BEFORE INSERT ON versions
                         WHEN NEW.kind='Artifact' AND NEW.spec->'source'->>'kind'='LocalSessionOutput'
                         BEGIN SELECT raise(ABORT,'withhold generated output'); END""")
        n.start()
        request = prepare_publication(n, token, self.session, allowed, "text/plain")
        refused = n.wait(n.call("Task.Submit", request, token)["task_id"])
        self.assertEqual((refused["lifecycle"], refused["outcome_knowledge"], refused["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "StorageUnavailable"), refused)
        n.stop()
        with sqlite3.connect(n.state / "state.db") as db:
            self.assertEqual(db.execute("SELECT count(*) FROM artifact_uploads").fetchone(), (0,))
            db.execute("DROP TRIGGER withhold_model_artifact")
        n.start()
        completed = finish_publication(n, token, prepare_publication(n, token, self.session, allowed, "text/plain"))
        n.stop()
        subprocess.run(n.daemon + ["verify"], check=True, timeout=15, stdout=subprocess.DEVNULL)
        artifact_id = completed["artifact_ref"]["resource_id"]
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("UPDATE versions SET spec=json_set(spec,'$.source.decision_digest',?) WHERE id=?",
                       ("sha256:" + "0" * 64, artifact_id))
        damaged = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=15)
        self.assertNotEqual(damaged.returncode, 0)
        self.assertIn("publication provenance is inconsistent", damaged.stderr)
        with sqlite3.connect(n.state / "state.db") as db:
            db.execute("""UPDATE versions SET spec=json_set(spec,'$.source.decision_digest',?,
                       '$.source.kind','LocalSubmission') WHERE id=?""", (completed["decision_digest"], artifact_id))
        rejected = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=15)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("provenance kind disagrees", rejected.stderr)
