"""Broker a fixed local input into a native Agent Session through the Unix API."""

from contextlib import closing
import json
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, key, target
from test_agent_session_inputs import SessionInputFixture
from test_schedule import after, timestamp


TOOL = "agent.session.start"


class BrokeredSessionInputs(SessionInputFixture, unittest.TestCase):
    def launch_grant(self, *, artifact=None, schedule=False, omit_action=None, omit_target=None):
        reference = artifact or self.artifact
        actions = [TOOL, "session.run", "session.context.read", "artifact.read", "node.inventory",
                   "task.submit", "task.read"]
        if schedule:
            actions.append("schedule.manage")
        targets = [self.node_id, self.model["resource_id"], self.skill["resource_id"], reference["resource_id"]]
        return self.grant(self.agent, [action for action in actions if action != omit_action],
                          [resource for resource in targets if resource != omit_target])

    def blueprint(self, artifact=None):
        return {"goal": "Read the local data identified by the fixed JSON input.", "skill_ref": self.skill,
                "model_ref": self.model, "context_policy": "ReferencesOnly", "max_turns": 3,
                "input_artifacts": [{"name": "approved-input", "reference": artifact or self.artifact,
                                     "kind": "Json"}]}

    def prepare_launch(self, grant, blueprint=None):
        return self.n.prepare(TOOL, self.blueprint() if blueprint is None else blueprint,
                              token=self.agent["token"], delegation=grant["delegation_ref"]["resource_id"])

    def scheduling(self, mode):
        node = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})

    def verify_stopped(self):
        report = json.loads(subprocess.check_output([*self.n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(report["database_format"], 106, report)
        self.assertFalse(report["migration_required"], report)

    def read_state(self):
        state = self.n.state / "state.db"
        return closing(sqlite3.connect(f"{state.as_uri()}?mode=ro", uri=True, timeout=10))

    def assert_custody(self, *, task_id=None, session_id=None):
        with self.read_state() as conn:
            artifact_id = self.artifact["resource_id"]
            task_holds = conn.execute("SELECT task,artifact FROM brokered_session_input_holds").fetchall()
            session_holds = conn.execute("SELECT session,artifact FROM session_input_holds").fetchall()
            self.assertEqual(task_holds, [(task_id, artifact_id)] if task_id else [])
            self.assertEqual(session_holds, [(session_id, artifact_id)] if session_id else [])

    def assert_no_session_effect(self, task_id=None):
        self.assertEqual(self.queued(key(), accept=True), {"available": False})
        self.assert_custody()
        with self.read_state() as conn:
            self.assertEqual(conn.execute("SELECT count(*) FROM versions WHERE kind='Session'").fetchone(), (0,))
            self.assertEqual(conn.execute("SELECT count(*) FROM session_turns").fetchone(), (0,))
            self.assertEqual(conn.execute("SELECT count(*) FROM model_queue").fetchone(), (0,))
            if task_id is not None:
                receipts = conn.execute("SELECT count(*) FROM versions WHERE kind='Artifact' AND spec->>'task_id'=?",
                                        (task_id,)).fetchone()
                self.assertEqual(receipts, (0,))

    def launch_receipt(self, task_id):
        task = self.n.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        receipt_ref, = task["result"]["artifact_refs"]
        output = self.n.call("Artifact.Read", {"reference": receipt_ref}, self.agent["token"])
        session = output["session_ref"]
        self.assertEqual(output, {"session_ref": session})
        projected = self.session(session)
        self.assertEqual(projected["reference"], session)
        origin = projected["session"]["origin_task_ref"]
        self.assertEqual(origin["resource_id"], task_id)
        self.assertEqual(self.n.call("Resource.Resolve", {"reference": origin})["reference"], origin)
        self.assertNotIn(self.data["private_ticket"], json.dumps([task, output, projected]))
        self.assert_custody(session_id=session["resource_id"])
        return session, receipt_ref

    def claim_exact_input(self, session, claim_key):
        claim = self.queued(claim_key, accept=True, wait_ms=3000)
        self.assertEqual(claim["session_id"], session["resource_id"], claim)
        self.assertEqual(claim["context"]["inputs"], [
            {"name": "approved-input", "reference": self.artifact, "kind": "Json", "value": self.data},
        ])
        return claim

    def test_queued_task_transfers_exact_hold_once_across_restart_and_runs_inventory(self):
        n = self.n
        caller = self.agent["token"]
        grant = self.launch_grant()
        self.scheduling("Cordoned")

        # The alternate exact input is independently withdrawable while it exists only in a draft.
        alternate = self.upload(b'{"only_prepared":true}', self.agent)
        draft = self.prepare_launch(self.launch_grant(artifact=alternate), self.blueprint(alternate))
        self.assert_custody()
        withdrawn = n.call("Artifact.Withdraw", {"reference": alternate, "reason": "Never submitted"}, caller)
        self.assertEqual(withdrawn["artifact_ref"], alternate)
        self.refuse("ResultEvicted", "Task.Submit", draft, caller)
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])

        request = self.prepare_launch(grant)
        self.assert_custody()
        accepted = n.call("Task.Submit", request, caller)
        task_id = accepted["task_id"]
        queued = n.call("Task.Get", {"id": task_id})
        self.assertEqual((queued["lifecycle"], queued["outcome_knowledge"]), ("Queued", "NotStarted"), queued)
        operation_ref, = queued["operation_refs"]
        operation = n.call("Resource.Resolve", {"reference": operation_ref})["value"]
        plan = n.call("Resource.Resolve", {"reference": operation["plan_ref"]})["value"]
        self.assertIn(self.artifact, [condition["resource_ref"] for condition in plan["preconditions"]])
        self.assert_custody(task_id=task_id)
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), caller)
        self.assertEqual(self.queued("before-commit", accept=True), {"available": False})

        n.stop(signal.SIGKILL)
        # Restore the exact row after proving the stopped verifier detects lost custody.
        binding = (task_id, self.artifact["resource_id"])
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn:
            with conn:
                deletion = conn.execute("DELETE FROM brokered_session_input_holds WHERE task=? AND artifact=?",
                                        binding)
                self.assertEqual(deletion.rowcount, 1)
            try:
                invalid = subprocess.run([*n.daemon, "verify"], capture_output=True, text=True, timeout=20)
                self.assertNotEqual(invalid.returncode, 0, invalid.stdout)
                self.assertIn("RecoveryRequired", invalid.stderr)
            finally:
                with conn:
                    conn.execute("INSERT INTO brokered_session_input_holds(task,artifact) VALUES(?,?)", binding)
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Task.Submit", request, caller), {**accepted, "deduplicated": True})
        self.assert_custody(task_id=task_id)
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), caller)
        self.scheduling("Enabled")
        session, receipt_ref = self.launch_receipt(task_id)
        self.assertEqual(self.queued("without-private-input-support"), {"available": False})
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), caller)

        first = self.claim_exact_input(session, "read-input-from-launch")
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.queued("read-input-from-launch", accept=True), {**first, "recovered": True})
        tool = first["context"]["inputs"][0]["value"]["tool"]
        invocation = self.respond(session, first, {"action": "tool", "tool": tool, "input": {}})
        inventory_id = invocation["task_id"]
        self.assertNotEqual(inventory_id, task_id)
        inventory = n.wait(inventory_id)
        self.assertEqual(inventory["lifecycle"], "Succeeded", inventory)
        report = n.call("Artifact.Read", {"reference": inventory["result"]["artifact_refs"][0]}, caller)
        self.assertGreater(report["memory_bytes"]["MemTotal"], 0)
        final = self.claim_exact_input(session, "finish-launched-session")
        finished = self.respond(session, final, {"action": "finish", "summary": "Inventory recorded."})
        self.assertEqual(finished["lifecycle"], "Succeeded", finished)
        self.assertEqual(self.model_calls(grant), 2)
        self.assert_custody()
        n.call("Artifact.Withdraw", self.withdrawal(), caller)
        self.assertEqual(n.call("Task.Submit", request, caller), {**accepted, "deduplicated": True})
        self.assertEqual(n.call("Artifact.Read", {"reference": receipt_ref}, caller), {"session_ref": session})
        self.assertEqual(self.queued("no-second-launch", accept=True), {"available": False})
        n.stop()
        self.verify_stopped()

    def test_revoking_exact_disclosure_before_dispatch_releases_pending_custody(self):
        n = self.n
        caller = self.agent["token"]
        grant = self.launch_grant()
        # A second valid delegation deliberately cannot repair missing scope on the chosen one.
        replacement = self.launch_grant()
        for restricted in (self.launch_grant(omit_action="session.context.read"),
                           self.launch_grant(omit_target=self.artifact["resource_id"]),
                           self.launch_grant(omit_target=self.node_id)):
            with self.assertRaises(Failure) as rejected:
                n.call("Task.Submit", self.prepare_launch(restricted), caller)
            self.assertEqual(rejected.exception.code, "PermissionDenied", rejected.exception)
        fallback = self.register_model()
        self.refuse("UnsupportedCapability", "Task.Prepare", {
            "tool": TOOL, "input": {**self.blueprint(), "fallback_model_refs": [fallback]}, "key": key(),
            "delegation_id": grant["delegation_ref"]["resource_id"],
        }, caller)
        self.assert_no_session_effect()

        self.scheduling("Cordoned")
        accepted = n.call("Task.Submit", self.prepare_launch(grant), caller)
        task_id = accepted["task_id"]
        self.assert_custody(task_id=task_id)
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        n.stop(signal.SIGKILL)
        n.start()
        self.scheduling("Enabled")
        failed = n.wait(task_id)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                         ("Failed", "NotStarted", "PermissionDenied"), failed)
        self.assertEqual(self.model_calls(replacement), 0)
        self.assert_no_session_effect(task_id)
        n.call("Artifact.Withdraw", self.withdrawal(), caller)
        n.stop()
        self.verify_stopped()

    def test_launch_receipt_failure_rolls_back_session_and_releases_both_holds(self):
        n = self.n
        caller = self.agent["token"]
        grant = self.launch_grant()
        self.scheduling("Cordoned")
        request = self.prepare_launch(grant)
        accepted = n.call("Task.Submit", request, caller)
        task_id = accepted["task_id"]
        self.assert_custody(task_id=task_id)
        # Abort only the launch result insertion, after Session construction in the dispatch transaction.
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn, conn:
            literal, = conn.execute("SELECT quote(?)", (task_id,)).fetchone()
            conn.execute(f"""CREATE TRIGGER refuse_launch_receipt BEFORE INSERT ON versions
                WHEN NEW.kind='Artifact' AND NEW.spec->>'task_id'={literal}
                BEGIN SELECT RAISE(ABORT, 'injected launch receipt publication failure'); END""")
        try:
            self.scheduling("Enabled")
            failed = n.wait(task_id)
            self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"]),
                             ("Failed", "NotStarted", "StorageUnavailable"), failed)
            self.assertEqual(failed["result"]["artifact_refs"], [])
            self.assertEqual(self.model_calls(grant), 0)
            self.assert_no_session_effect(task_id)
        finally:
            with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn, conn:
                conn.execute("DROP TRIGGER refuse_launch_receipt")

        n.stop(signal.SIGKILL)
        self.verify_stopped()
        n.start()
        self.assertEqual(n.call("Task.Submit", request, caller), {**accepted, "deduplicated": True})
        self.assertEqual(n.call("Task.Get", {"id": task_id}), failed)
        self.assert_no_session_effect(task_id)
        n.call("Artifact.Withdraw", self.withdrawal(), caller)

    def test_timed_schedule_starts_one_session_with_exact_input_after_restart(self):
        n = self.n
        caller = self.agent["token"]
        grant = self.launch_grant(schedule=True)
        self.scheduling("Cordoned")
        request = {"key": key(), "tool": TOOL, "input": self.blueprint(), "run_at": timestamp(1.5),
                   "deadline_at": timestamp(35), "delegation_id": grant["delegation_ref"]["resource_id"]}
        scheduled = n.call("Schedule.Create", request, caller)
        schedule_id = scheduled["reference"]["resource_id"]
        self.assert_custody()
        n.stop(signal.SIGKILL)
        after(request["run_at"])
        n.start()
        bounds = {"id": schedule_id}
        self.assertEqual(n.call("Schedule.Get", bounds, caller)["state"]["lifecycle"], "Pending")
        self.scheduling("Enabled")

        deadline = time.monotonic() + 10
        while True:
            triggered = n.call("Schedule.Get", bounds, caller)["state"]
            if triggered["lifecycle"] != "Pending" or time.monotonic() >= deadline:
                break
            time.sleep(.05)
        self.assertEqual(triggered["lifecycle"], "Triggered", triggered)
        receipt = triggered["task_receipt"]
        self.assertEqual(receipt["origin_ref"]["origin_type"], "Schedule")
        self.assertEqual(receipt["origin_ref"]["origin_id"], schedule_id)
        session, _ = self.launch_receipt(receipt["task_id"])
        self.assertEqual(n.call("Schedule.Create", request, caller)["state"]["task_receipt"], receipt)
        self.refuse("Conflict", "Artifact.Withdraw", self.withdrawal(), caller)
        claim = self.claim_exact_input(session, "scheduled-private-input")
        finished = self.respond(session, claim, {"action": "finish", "summary": "Exact scheduled input received."})
        self.assertEqual(finished["lifecycle"], "Succeeded", finished)
        self.assert_custody()
        n.call("Artifact.Withdraw", self.withdrawal(), caller)
        self.assertEqual(self.queued("no-repeated-trigger", accept=True), {"available": False})
        n.stop()
        self.verify_stopped()
