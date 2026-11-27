"""Exercise bounded context, lease replay, and artifact retention through the native Unix API."""
from contextlib import closing
import json
import os
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target


class AgentToolContextFixture:
    bin_dir = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")

    def setUp(self):
        self.node = Node(self.bin_dir)
        self.addCleanup(self.node.close)
        self.tools = ["node.inventory", "system.status"]
        self.skill = self.node.call("Skill.Publish", {
            "name": "two-observations", "version": "1", "source": "local-test", "license": "0BSD",
            "instructions": "Collect node inventory and system status.", "tools": self.tools,
        })
        self.provider, self.model = self.register_provider()
        self.agent = self.node.call("Agent.Register", {"name": "context-consumer"})
        self.grant = self.grant_for([])

    def register_provider(self, max_context_bytes=131072):
        provider, _ = self.node.agent(["model.serve"])
        model = self.node.call("Model.Register", {
            "name": "context-provider", "revision": key(), "provider_principal_id": provider["principal_id"],
            "max_context_bytes": max_context_bytes, "max_output_bytes": 1024, "lease_seconds": 60,
            "max_in_flight": 1,
        })
        return provider, model

    def grant_for(self, additional, targets=None):
        request = {"principal_id": self.agent["principal_id"], "units": 16, "model_calls": 8,
                   "actions": self.tools + ["session.run", "task.submit", "task.read", *additional],
                   "ttl_seconds": 300}
        if targets is not None:
            request["tool_targets"] = targets
        return self.node.call("Grant.Issue", request)

    def opening(self, context_policy="LocalToolResults", grant=None, **kwargs):
        selected = grant or self.grant
        return {"key": key(), "goal": "Summarize both tool observations.", "skill_ref": self.skill,
                "model_ref": self.model, "context_policy": context_policy, "max_turns": 5,
                "provider_queue": True, "delegation_id": selected["delegation_ref"]["resource_id"], **kwargs}

    def open(self, **kwargs):
        return self.node.call("Session.Open", self.opening(**kwargs), self.agent["token"])["reference"]

    def claim(self, claim_key=None, provider=None, model=None):
        return self.node.call("Model.ClaimNext", {"key": claim_key or key(), "model_ref": model or self.model},
                              (provider or self.provider)["token"])

    def decision(self, claim, decision, provider=None):
        return self.node.call("Model.Respond", {"session_id": claim["session_id"], "lease_id": claim["lease_id"],
                                               "decision": decision}, (provider or self.provider)["token"])

    def observe(self, claim, tool):
        response = self.decision(claim, {"action": "tool", "tool": tool, "input": {}})
        task_id = response["task_id"]
        status = self.node.wait(task_id)
        self.assertEqual(status["lifecycle"], "Succeeded", status)
        artifact, = status["result"]["artifact_refs"]
        return task_id, artifact

    def evict(self, task):
        return self.node.call("Task.EvictResults", {**target(self.node.get(task)["reference"]),
                                                  "reason": "test retention window"})

    def budget(self):
        return self.node.call("Budget.Get", {"id": self.grant["budget_ref"]["resource_id"]},
                              self.agent["token"])["model_calls_used"]

    def direct_claim(self, session, claim_key):
        return self.node.call("Model.Claim", {"session_id": session["resource_id"], "key": claim_key},
                              self.provider["token"])


class AgentToolContext(AgentToolContextFixture, unittest.TestCase):
    def test_default_replay_preserves_issued_task_view_after_real_eviction(self):
        n = self.node
        token = self.agent["token"]
        for queued in (False, True):
            with self.subTest(queued=queued):
                session = self.open(context_policy="ReferencesOnly", provider_queue=queued)

                def claim(claim_key):
                    if queued:
                        return self.claim(claim_key)
                    return {**self.direct_claim(session, claim_key), "session_id": session["resource_id"]}

                used = self.budget()
                tool, result = self.observe(claim(key()), "system.status")
                second_key = key()
                issued = claim(second_key)
                self.assertNotIn("context_digest", issued)
                self.assertNotIn("previous_tools", issued["context"])
                self.assertEqual(issued["context"]["last_tool"]["result"], {
                    "availability": "Available", "artifact_refs": [result],
                })
                self.evict(tool)
                status = n.call("Task.Get", {"id": tool}, token)
                self.assertEqual(status["result"]["availability"], "Evicted")
                self.assertEqual(self.budget(), used + 2)
                n.stop(signal.SIGKILL)
                if queued:
                    with closing(sqlite3.connect(n.state / "state.db")) as conn:
                        with conn:
                            saved = conn.execute(
                                "SELECT lease,session_ref,usage_ref,context_digest FROM model_context_snapshots "
                                "WHERE lease=?", (issued["lease_id"],),
                            ).fetchone()
                            self.assertEqual(saved[0], issued["lease_id"])
                            conn.execute("DELETE FROM model_context_snapshots WHERE lease=?", (saved[0],))
                        try:
                            invalid = subprocess.run(n.daemon + ["verify"], capture_output=True, timeout=20)
                            self.assertNotEqual(invalid.returncode, 0, invalid.stdout)
                            self.assertIn(b"default model lease checkpoint disagrees with its issuance format",
                                          invalid.stderr)
                        finally:
                            with conn:
                                conn.execute(
                                    "INSERT INTO model_context_snapshots(lease,session_ref,usage_ref,context_digest) "
                                    "VALUES(?,?,?,?)", saved,
                                )
                subprocess.run(n.daemon + ["verify"], check=True, capture_output=True, timeout=20)
                n.start()
                replay = claim(second_key)
                self.assertEqual(replay, {**issued, "recovered": True})
                self.assertEqual(self.budget(), used + 2)
                receipt = self.decision(replay, {"action": "finish", "summary": "Reviewed original observation."})
                self.assertEqual(receipt["lifecycle"], "Succeeded")
                completed = claim(second_key)
                self.assertEqual(completed["completed"], receipt)
                self.assertNotIn("context", completed)
                history = n.call("Session.History", {"id": session["resource_id"]}, token)
                self.assertEqual(len(history["tasks"]), 1)
                self.assertNotIn("context_digest", history["turns"][-1])

    def test_two_tools_replay_exactly_after_crash_and_release_expired_retention(self):
        n = self.node
        session = self.open(tool_context_window=2, max_turns=4)
        n.call("Session.Extend", {**target(session), "max_turns": 5}, self.agent["token"])
        first_claim = self.claim("first")
        self.assertEqual(first_claim["context"]["previous_tools"], [])
        self.assertIsNone(first_claim["context"]["last_tool"])
        self.assertRegex(first_claim["context_digest"], r"^sha256:[0-9a-f]{64}$")
        first, first_ref = self.observe(first_claim, self.tools[0])
        second_claim = self.claim("second")
        second, second_ref = self.observe(second_claim, self.tools[1])
        with self.assertRaises(Failure) as pinned:
            self.evict(first)
        self.assertEqual(pinned.exception.code, "Conflict")

        third = self.claim("third")
        first_observation, = third["context"]["previous_tools"]
        latest = third["context"]["last_tool"]
        self.assertEqual((first_observation["task_id"], first_observation["result"]["artifact_refs"]),
                         (first, [first_ref]))
        self.assertEqual((latest["task_id"], latest["result"]["artifact_refs"]), (second, [second_ref]))
        self.assertEqual((first_observation["turn"], latest["turn"]), (1, 2))
        for observation, ref in [(first_observation, first_ref), (latest, second_ref)]:
            expected = n.call("Artifact.Read", {"reference": ref}, self.agent["token"])
            self.assertEqual(observation["local_tool_result"], expected)
        self.assertEqual(self.budget(), 3)

        n.stop(signal.SIGKILL)
        db_path = n.state / "state.db"
        with closing(sqlite3.connect(db_path)) as c, c:
            c.execute("UPDATE model_context_snapshots SET context_digest=? WHERE lease=?",
                      ("sha256:" + "0" * 64, third["lease_id"]))
        invalid = subprocess.run(n.daemon + ["verify"], timeout=20, capture_output=True)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn(b"model lease context disagrees with its original session and usage", invalid.stderr)
        with closing(sqlite3.connect(db_path)) as c, c:
            c.execute("UPDATE model_context_snapshots SET context_digest=? WHERE lease=?",
                      (third["context_digest"], third["lease_id"]))
        subprocess.run(n.daemon + ["verify"], timeout=20, check=True, capture_output=True)

        chunk = (first_ref["resource_id"],)
        with closing(sqlite3.connect(db_path)) as c, c:
            original, = c.execute("SELECT payload FROM artifact_chunks WHERE artifact_id=? AND chunk_index=0",
                                  chunk).fetchone()
            c.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload)) "
                      "WHERE artifact_id=? AND chunk_index=0", chunk)
        n.start()
        with self.assertRaises(Failure) as changed:
            self.claim("third")
        self.assertEqual(changed.exception.code, "ResultUnavailable")
        self.assertEqual(self.budget(), 3)
        n.stop()
        with closing(sqlite3.connect(db_path)) as c, c:
            c.execute("UPDATE artifact_chunks SET payload=? WHERE artifact_id=? AND chunk_index=0",
                      (original, *chunk))
        n.start()
        replay = self.claim("third")
        self.assertEqual(replay, {**third, "recovered": True})

        new_task, _ = self.observe(replay, self.tools[0])
        evicted = self.evict(first)
        self.assertEqual(evicted["eviction"]["artifact_refs"], [first_ref])
        with self.assertRaises(Failure) as pinned:
            self.evict(second)
        self.assertEqual(pinned.exception.code, "Conflict")
        fourth = self.claim("fourth")
        self.assertEqual([item["task_id"] for item in fourth["context"]["previous_tools"]], [second])
        self.assertEqual(fourth["context"]["last_tool"]["task_id"], new_task)
        self.decision(fourth, {"action": "finish", "summary": "reviewed both observations"})
        self.evict(second)
        self.evict(new_task)
        history = n.call("Session.History", {"id": session["resource_id"]}, self.agent["token"])
        self.assertEqual([item["task_id"] for item in history["tasks"]], [first, second, new_task])
        self.assertEqual(history["turns"][2]["context_digest"], third["context_digest"])
        n.stop()
        with closing(sqlite3.connect(db_path)) as c, c:
            stored, = c.execute("SELECT session_ref FROM model_context_snapshots WHERE lease=?",
                                (first_claim["lease_id"],)).fetchone()
            self.assertNotEqual(json.loads(stored), session)
            c.execute("UPDATE model_context_snapshots SET session_ref=? WHERE lease=?",
                      (json.dumps(session), first_claim["lease_id"]))
        invalid = subprocess.run(n.daemon + ["verify"], timeout=20, capture_output=True)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn(b"model lease context disagrees with its original session and usage", invalid.stderr)
        with closing(sqlite3.connect(db_path)) as c, c:
            c.execute("UPDATE model_context_snapshots SET session_ref=? WHERE lease=?",
                      (stored, first_claim["lease_id"]))
        subprocess.run(n.daemon + ["verify"], timeout=20, check=True, capture_output=True)

    def test_retry_drops_only_verified_evictions_from_the_failed_sessions_window(self):
        n = self.node
        self.tools.append("volume.read")
        self.skill = n.call("Skill.Publish", {
            "name": "retry-context", "version": "1", "source": "local-test", "license": "0BSD",
            "instructions": "Collect two observations before reading the owned volume.", "tools": self.tools,
        })
        self.grant = self.grant_for(["volume.create"])
        token = self.agent["token"]
        delegation = self.grant["delegation_ref"]["resource_id"]
        created = n.prepare("volume.create", {"name": "retry-context-volume"}, token=token, delegation=delegation)
        made = n.wait(n.call("Task.Submit", created, token)["task_id"])
        self.assertEqual(made["lifecycle"], "Succeeded", made)
        volume, = made["result"]["artifact_refs"]
        volume = n.call("Artifact.Read", {"reference": volume}, token)
        volume = n.perform("volume.write", {**target(volume), "name": "record", "content": "ready"})["volume_ref"]
        directory = n.state / "volumes" / volume["resource_id"] / "data"
        hidden = directory.with_name("temporarily-unmounted")

        session = self.open(tool_context_window=3)
        evicted, _ = self.observe(self.claim(), self.tools[0])
        retained, retained_ref = self.observe(self.claim(), self.tools[1])
        lease = self.claim()
        directory.rename(hidden)
        try:
            failing = self.decision(lease, {"action": "tool", "tool": "volume.read",
                                           "input": {**target(volume), "name": "record"}})["task_id"]
            self.assertEqual(n.wait(failing)["lifecycle"], "Failed")
        finally:
            hidden.rename(directory)
        state = n.call("Session.Get", {"id": session["resource_id"]}, token)
        self.assertEqual(state["session"]["lifecycle"], "Failed")
        self.evict(evicted)
        retry = {**target(state["reference"]), "task_id": failing, "key": key()}

        n.stop()
        db_path = n.state / "state.db"
        chunk = (retained_ref["resource_id"],)
        with closing(sqlite3.connect(db_path)) as c, c:
            original, = c.execute("SELECT payload FROM artifact_chunks WHERE artifact_id=? AND chunk_index=0",
                                  chunk).fetchone()
            c.execute("UPDATE artifact_chunks SET payload=zeroblob(length(payload)) "
                      "WHERE artifact_id=? AND chunk_index=0", chunk)
        n.start()
        with self.assertRaises(Failure) as unavailable:
            n.call("Session.RetryTask", retry, token)
        self.assertEqual(unavailable.exception.code, "ResultUnavailable")
        self.assertEqual(n.call("Session.Get", {"id": session["resource_id"]}, token), state)
        n.stop()
        with closing(sqlite3.connect(db_path)) as c, c:
            c.execute("UPDATE artifact_chunks SET payload=? WHERE artifact_id=? AND chunk_index=0",
                      (original, *chunk))
        n.start()
        retried = n.call("Session.RetryTask", retry, token)["task_id"]
        succeeded = n.wait(retried)
        self.assertEqual(succeeded["lifecycle"], "Succeeded", succeeded)
        self.assertEqual(n.call("Session.RetryTask", retry, token), {"task_id": retried, "deduplicated": True})
        next_turn = self.claim()
        previous, = next_turn["context"]["previous_tools"]
        self.assertEqual(previous["task_id"], retained)
        latest = next_turn["context"]["last_tool"]
        self.assertEqual((latest["task_id"], latest["attempt"], latest["local_tool_result"]["content"]),
                         (retried, 2, "ready"))
        n.stop()
        subprocess.run(n.daemon + ["verify"], timeout=20, check=True, capture_output=True)

    def test_reference_window_preserves_order_across_explicit_scoped_model_handoff(self):
        n = self.node
        alternative, model = self.register_provider()
        node_id = n.call("Node.Get", {})["reference"]["resource_id"]
        grant = self.grant_for(["session.failover"], ["profile-local", node_id,
                               self.model["resource_id"], model["resource_id"], self.skill["resource_id"]])
        session = self.open(context_policy="ReferencesOnly", grant=grant, tool_context_window=3,
                            fallback_model_refs=[model])
        first, first_ref = self.observe(self.claim(), self.tools[0])
        second, second_ref = self.observe(self.claim(), self.tools[1])
        self.evict(first)
        status = n.call("Session.Get", {"id": session["resource_id"]}, self.agent["token"])
        transferred = n.call("Session.UseFallback", {**target(status["reference"]), "model_ref": model},
                             self.agent["token"])["successor"]["reference"]
        current = self.claim(provider=alternative, model=model)
        self.assertEqual(current["session_id"], transferred["resource_id"])
        earlier, = current["context"]["previous_tools"]
        latest = current["context"]["last_tool"]
        self.assertEqual((earlier["task_id"], earlier["result"]["artifact_refs"]), (first, [first_ref]))
        self.assertEqual((latest["task_id"], latest["result"]["artifact_refs"]), (second, [second_ref]))
        self.assertNotIn("local_tool_result", earlier)
        self.assertNotIn("local_tool_result", latest)
        self.decision(current, {"action": "finish", "summary": "observed two references"}, alternative)

    def test_legacy_context_unchanged_and_opt_in_bounds_checked(self):
        n = self.node
        for window in [0, 5]:
            with self.assertRaises(Failure) as invalid:
                n.call("Session.Open", self.opening(tool_context_window=window), self.agent["token"])
            self.assertEqual(invalid.exception.code, "InvalidArgument")
        self.open()
        current = self.claim()
        self.assertNotIn("context_digest", current)
        self.assertNotIn("previous_tools", current["context"])

    def test_oversized_local_context_is_paused_across_restart_without_another_charge(self):
        n = self.node
        self.provider, self.model = self.register_provider(max_context_bytes=1100)
        session = self.open(tool_context_window=2)
        settled, _ = self.observe(self.claim(), self.tools[0])
        self.assertEqual(self.claim(), {"available": False})
        paused = n.call("Session.Get", {"id": session["resource_id"]}, self.agent["token"])
        self.assertEqual((paused["session"]["lifecycle"], paused["session"]["pause_reason"]),
                         ("Paused", "ModelContextLimit"))
        self.assertEqual(self.budget(), 1)
        self.assertEqual(paused["session"]["last_task"], settled)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.claim(), {"available": False})
        self.assertEqual(self.budget(), 1)

    def test_damaged_session_context_does_not_block_another_provider_claim(self):
        n = self.node
        damaged = self.open(tool_context_window=2)
        self.observe(self.claim(), self.tools[0])
        healthy = self.open(tool_context_window=2)
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as c, c:
            c.execute("UPDATE versions SET body=json_set(body,'$.tool_context_refs[0].source_session_id','unrelated') "
                      "WHERE id=? AND rev=(SELECT max(rev) FROM versions WHERE id=?)",
                      (damaged["resource_id"], damaged["resource_id"]))
        n.start()
        available = self.claim()
        self.assertEqual(available["session_id"], healthy["resource_id"])
        paused = n.call("Session.Get", {"id": damaged["resource_id"]}, self.agent["token"])
        self.assertEqual((paused["session"]["lifecycle"], paused["session"]["pause_reason"]),
                         ("Paused", "ModelContextCheckpointInvalid"))
        self.assertEqual(self.budget(), 2)


if __name__ == "__main__":
    unittest.main()
