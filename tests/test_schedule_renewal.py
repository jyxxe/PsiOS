"""Renew finite schedules through live daemon, scope, recovery and execution paths."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from test_schedule import after, timestamp


class ScheduleRenewalTests(unittest.TestCase):
    def setUp(self):
        self.n = Node()
        self.addCleanup(self.n.close)

    def scope(self, actions, targets, model_calls=0, principal_id="local-owner"):
        return self.n.call("Grant.Issue", {"principal_id": principal_id, "units": 12,
                          "ttl_seconds": 180, "model_calls": model_calls, "tool_targets": targets,
                          "actions": ["schedule.manage", "task.submit", "task.read", *actions]})

    def series(self, tool, tool_input, scope, overlap="Allow", token=None, missed=None):
        request = {"key": key(), "tool": tool, "input": tool_input,
                   "delegation_id": scope["delegation_ref"]["resource_id"],
                   "run_at": timestamp(1), "deadline_at": timestamp(20),
                   "repeat": {"interval_seconds": 1, "count": 2,
                              "missed": missed or {"mode": "BoundedCatchUp", "limit": 2}, "overlap": overlap}}
        return self.n.call("Schedule.Create", request, token)["reference"]

    def request(self, reference, scope, delay=10):
        return {**target(reference), "key": key(), "delegation_id": scope["delegation_ref"]["resource_id"],
                "run_at": timestamp(delay), "deadline_at": timestamp(delay + 18)}

    def get(self, reference):
        return self.n.call("Schedule.Get", {"id": reference["resource_id"]})

    def wait_until(self, read, reached, seconds=15):
        deadline = time.monotonic() + seconds
        while True:
            actual = read()
            if reached(actual):
                return actual
            self.assertLess(time.monotonic(), deadline, actual)
            time.sleep(0.03)

    def complete(self, reference):
        current = self.wait_until(lambda: self.get(reference), lambda view: view["state"]["lifecycle"] == "Completed")
        triggers = [self.n.call("Schedule.TriggerGet", {"id": trigger["resource_id"]})["state"]
                    for trigger in current["state"]["trigger_refs"]]
        self.assertEqual(len(triggers), 2)
        task_ids = [trigger["task_receipt"]["task_id"] for trigger in triggers if trigger["lifecycle"] == "Triggered"]
        return current, task_ids

    def refuse(self, code, request):
        with self.assertRaises(Failure) as denied:
            self.n.call("Schedule.Renew", request)
        self.assertEqual(denied.exception.code, code, denied.exception)

    def database(self, statement):
        self.n.stop()
        with closing(sqlite3.connect(self.n.state / "state.db")) as db, db:
            db.execute(statement)
        self.n.start()

    def result(self, task_id):
        complete = self.n.wait(task_id)
        self.assertEqual((complete["lifecycle"], complete["outcome_knowledge"]), ("Succeeded", "Known"), complete)
        artifact, = complete["result"]["artifact_refs"]
        return self.n.call("Artifact.Read", {"reference": artifact})

    def test_successor_admission_rolls_back_on_receipt_fault_and_is_unique_after_restart(self):
        n = self.n
        scope = self.scope(["system.status"], ["profile-local"])
        predecessor = self.series("system.status", {}, scope)
        self.refuse("Conflict", self.request(predecessor, scope))
        previous, task_ids = self.complete(predecessor)
        self.assertEqual(len(task_ids), 2)
        for task_id in task_ids:
            self.result(task_id)
        request = self.request(previous["reference"], scope, delay=8)
        self.database("""CREATE TRIGGER withhold_schedule_renewal BEFORE INSERT ON versions
                         WHEN NEW.kind='ScheduleRenewal' BEGIN SELECT RAISE(ABORT, 'test renewal loss'); END""")
        self.refuse("StorageUnavailable", request)
        self.assertEqual(n.call("Resource.List", {"kind": "Schedule"})["items"], [previous["reference"]])
        self.assertEqual(self.get(predecessor), previous)
        self.database("DROP TRIGGER withhold_schedule_renewal")

        def attempt(payload):
            try:
                return payload, n.call("Schedule.Renew", payload)
            except Failure as error:
                self.assertEqual(error.code, "IdempotencyConflict", error)
                return payload, None

        with ThreadPoolExecutor(max_workers=2) as executor:
            results = list(executor.map(attempt, [request, {**request, "key": key()}]))
        (accepted, renewal), = [result for result in results if result[1] is not None]
        successor = renewal["schedule_ref"]
        facts = self.get(successor)["state"]
        self.assertEqual(facts["predecessor_ref"], previous["reference"])
        self.assertEqual(facts["repeat"], previous["state"]["repeat"])
        self.assertEqual(self.get(predecessor), previous)
        n.stop(signal.SIGKILL)
        check = json.loads(subprocess.check_output([*n.daemon, "verify", "--scratch-dir", n.base], timeout=20))
        self.assertEqual(check["database_format"], 131)
        n.start()
        repeat = subprocess.check_output([n.binaries / "psiosctl", "--config", n.config, "schedule-renew"],
                                         input=json.dumps(accepted).encode(), timeout=15)
        self.assertEqual(json.loads(repeat), renewal)
        _, jobs = self.complete(successor)
        self.assertEqual(len(jobs), 2)
        self.assertFalse(set(task_ids) & set(jobs))
        for task in jobs:
            self.assertTrue(self.result(task)["authority_id"])

    def test_earlier_pending_effect_and_new_scope_both_gate_renewal(self):
        n = self.n
        volume = n.perform("volume.create", {"name": "scheduled-write-renewal"})
        scope = self.scope(["volume.write"], [volume["resource_id"]])
        write = {**target(volume), "name": "scheduled-entry", "content": "requires approval"}
        previous, ids = self.complete(self.series("volume.write", write, scope))
        earlier, later = ids
        self.assertEqual(n.wait(earlier)["lifecycle"], "WaitingApproval")
        n.call("Task.Cancel", target(n.get(later)["reference"]))
        self.assertEqual(n.wait(later)["lifecycle"], "Cancelled")
        request = self.request(previous["reference"], scope)
        self.refuse("Conflict", request)
        n.call("Task.Cancel", target(n.get(earlier)["reference"]))
        self.assertEqual(n.wait(earlier)["lifecycle"], "Cancelled")
        other = n.perform("volume.create", {"name": "other-schedule-scope"})
        foreign = self.scope(["volume.write"], [other["resource_id"]])
        self.refuse("PermissionDenied", {**request, "delegation_id": foreign["delegation_ref"]["resource_id"]})
        self.assertTrue(n.call("Schedule.Renew", request)["renewal_ref"])

    def test_linked_workflow_must_finish_before_renewal(self):
        n = self.n
        volume = n.perform("volume.create", {"name": "scheduled-workflow-renewal"})
        self.scope(["workflow.manage"], [volume["resource_id"]])
        definition = n.call("Workflow.Define", {"key": key(), "steps": [{
            "id": "record", "tool": "volume.write", "after": [],
            "input": {**target(volume), "name": "confirm", "content": "approval required"},
        }]})["reference"]
        workflow_scope = self.scope(["workflow.manage", "workflow.start", "volume.write"],
                                    [volume["resource_id"], definition["resource_id"]])
        workflow_schedule = self.series("workflow.start", {"definition_ref": definition, "max_runtime_seconds": 23},
                                        workflow_scope, overlap="SkipWhileWorkflowActive")
        previous, jobs = self.complete(workflow_schedule)
        launch, = jobs
        run = self.result(launch)["workflow_run_ref"]
        request = self.request(previous["reference"], workflow_scope)
        self.refuse("Conflict", request)
        workflow = n.call("Workflow.Get", {"id": run["resource_id"]})
        n.call("Workflow.Cancel", target(workflow["reference"]))
        self.wait_until(lambda: n.call("Workflow.Get", {"id": run["resource_id"]}),
                        lambda view: view["state"]["lifecycle"] == "Cancelled")
        self.assertTrue(n.call("Schedule.Renew", request)["schedule_ref"])

    def test_successor_belongs_to_the_original_principal(self):
        n = self.n
        actor = n.call("Agent.Register", {"name": "schedule-renewal-owner"})
        scope = self.scope(["system.status"], ["profile-local"], principal_id=actor["principal_id"])
        original = self.series("system.status", {}, scope, token=actor["token"])
        previous, jobs = self.complete(original)
        self.assertEqual(len(jobs), 2)
        for job in jobs:
            self.assertEqual(n.wait(job)["lifecycle"], "Succeeded")
        request = self.request(previous["reference"], scope)
        self.refuse("PermissionDenied", request)
        renewal = n.call("Schedule.Renew", request, actor["token"])
        self.assertTrue(renewal["schedule_ref"])
        self.refuse("PermissionDenied", request)
        self.assertEqual(n.call("Schedule.Renew", request, actor["token"]), renewal)

    def test_coalesced_occurrences_keep_one_exact_settlement(self):
        n = self.n
        self.scheduling("Cordoned")
        scope = self.scope(["system.status"], ["profile-local"])
        original = self.series("system.status", {}, scope, missed={"mode": "Coalesce"})
        after(self.get(original)["state"]["next"]["run_at"], 1.25)
        self.scheduling("Enabled")
        previous, jobs = self.complete(original)
        job, = jobs
        self.result(job)
        renewed = n.call("Schedule.Renew", self.request(previous["reference"], scope))
        receipt = n.get(renewed["renewal_ref"]["resource_id"])["value"]
        self.assertEqual(receipt["settled_task_refs"], [n.get(job)["reference"]])
        self.assertEqual(self.get(renewed["schedule_ref"])["state"]["repeat"]["missed"], {"mode": "Coalesce"})

    def test_linked_agent_sessions_must_finish_before_renewal(self):
        n = self.n
        provider, _ = n.agent(["model.serve"])
        model = n.call("Model.Register", {"name": "renewal-model", "revision": "1",
            "provider_principal_id": provider["principal_id"], "lease_seconds": 20, "max_in_flight": 1,
            "max_context_bytes": 32768, "max_output_bytes": 8192, "inference_outcome_policy": "Strict"})
        skill = n.call("Skill.Publish", {"name": "renewal-skill", "version": "1", "source": "local-test",
            "license": "0BSD", "instructions": "Read current service status", "tools": ["system.status"]})
        session_scope = self.scope(["agent.session.start", "session.run", "system.status"],
                                   [model["resource_id"], skill["resource_id"], "profile-local"], model_calls=3)
        schedule = self.series("agent.session.start", {"goal": "Monitor system status", "skill_ref": skill,
                               "model_ref": model, "context_policy": "ReferencesOnly", "max_turns": 2}, session_scope)
        previous, jobs = self.complete(schedule)
        sessions = [self.result(task)["session_ref"] for task in jobs]
        self.assertEqual(len(sessions), 2)
        request = self.request(previous["reference"], session_scope)
        self.refuse("Conflict", request)
        for session in sessions:
            current = n.call("Session.Get", {"id": session["resource_id"]})
            n.call("Session.Cancel", target(current["reference"]))
        self.assertTrue(n.call("Schedule.Renew", request)["schedule_ref"])

    @unittest.skipUnless(os.getenv("PSIOS_SCHEDULE_RENEWAL_PRIOR_BIN_DIR"), "requires the format 130 binaries")
    def test_format_130_pending_occurrences_keep_their_original_authority(self):
        current_binaries = self.n.binaries
        self.n.close()
        self.n = n = Node(os.environ["PSIOS_SCHEDULE_RENEWAL_PRIOR_BIN_DIR"])
        self.addCleanup(n.close)

        self.scheduling("Cordoned")
        scope = self.scope(["system.status"], ["profile-local"])
        original = self.series("system.status", {}, scope)
        prior = self.get(original)
        n.stop(signal.SIGKILL)
        previous = n.daemon[:]

        def verify(command):
            return json.loads(subprocess.check_output([*command, "verify", "--scratch-dir", n.base], timeout=20))

        old = verify(previous)
        self.assertEqual((old["database_format"], old["migration_required"]), (130, False))
        n.daemon[0] = str(current_binaries / "psiosd")
        staged = verify(n.daemon)
        self.assertEqual((staged["node_id"], staged["database_format"], staged["migration_required"]),
                         (old["node_id"], 130, True))
        n.start()
        self.assertEqual(self.get(original), prior)
        self.scheduling("Enabled")
        completed, tasks = self.complete(original)
        self.assertEqual(len(tasks), 2)
        for task in tasks:
            self.result(task)
        renewal = n.call("Schedule.Renew", self.request(completed["reference"], scope))
        self.assertEqual(self.get(renewal["schedule_ref"])["state"]["predecessor_ref"], completed["reference"])
        n.stop(signal.SIGKILL)
        migrated = verify(n.daemon)
        self.assertEqual((migrated["node_id"], migrated["database_format"], migrated["migration_required"]),
                         (old["node_id"], 131, False))
        refused = subprocess.run([*previous, "serve"], capture_output=True, text=True, timeout=15)
        self.assertNotEqual(refused.returncode, 0, refused)
        self.assertIn("SchemaVersionUnsupported", refused.stderr)

    def scheduling(self, mode):
        node = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})
