"""Exercise the durable node work gate without conflating it with worker scheduling."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import subprocess
from threading import Barrier
from time import monotonic, sleep
import unittest

from support import Failure, Node, key, target
from test_model_fallback import publish_skill
from test_recovery_admission import lose_restore_receipt
from test_schedule import after, timestamp


GET = "Node.WorkAdmission.Get"
SET = "Node.WorkAdmission.Set"
QUIESCENCE = "Node.WorkAdmission.Quiescence"
WAIT_QUIESCENT = "Node.WorkAdmission.WaitQuiescent"
STATUS = {"action": "tool", "tool": "system.status", "input": {}}


class NodeWorkAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def rejected(self, code, method, request, token=None, retry=None):
        with self.assertRaises(Failure) as caught:
            self.node.call(method, request, token)
        self.assertEqual(caught.exception.code, code, caught.exception)
        if retry is not None:
            self.assertEqual(caught.exception.args[0]["retry_condition"], retry, caught.exception)

    def blocked(self, method, request, token=None):
        self.rejected("PreconditionFailed", method, request, token, retry="SameRequestAfterBackoff")

    def admission(self):
        return self.node.call(GET, {})

    def change(self, state, mode, reason):
        return self.node.call(SET, {"expected_resource_version": state["reference"]["version"],
                                    "mode": mode, "reason": reason})

    def scheduling(self, mode):
        node = self.node.call("Node.Get", {})["reference"]
        self.node.call("Node.Configure", {
            "scheduling": mode, "expected_resource_version": node["version"],
        })

    def inventory(self, *kinds):
        return {
            kind: {ref["resource_id"] for ref in self.node.call("Resource.List", {"kind": kind})["items"]}
            for kind in kinds
        }

    def task_result(self, task_id, token=None):
        task = self.node.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        ref, = task["result"]["artifact_refs"]
        return self.node.call("Artifact.Read", {"reference": ref}, token)

    def restart(self):
        self.node.stop(signal.SIGKILL)
        self.node.start()

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertIs(report["migration_required"], False, report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"], report["witness"]),
                         ("Verified", "Verified", "Confirmed"), report)

    def test_quiescence_wait_uses_the_exact_draining_revision_and_real_task_settlement(self):
        n = self.node
        opening = self.admission()
        initial = {"expected_resource_version": opening["reference"]["version"]}
        self.rejected("PreconditionFailed", QUIESCENCE, initial)
        self.rejected("PreconditionFailed", WAIT_QUIESCENT, {**initial, "timeout_ms": 50})
        self.scheduling("Cordoned")
        tasks = [n.submit("system.status", {}) for _ in range(2)]
        draining = self.change(opening, "Draining", "wait for both accepted tasks")
        revision = draining["reference"]["version"]
        query = {"expected_resource_version": revision, "limit": 1}
        observer, _ = n.agent(["task.read", "resource.read"])
        self.rejected("PermissionDenied", QUIESCENCE, query, observer["token"])
        self.rejected("PermissionDenied", WAIT_QUIESCENT, {**query, "timeout_ms": 50}, observer["token"])
        for value in (0, 10001, True, "100"):
            self.rejected("InvalidArgument", WAIT_QUIESCENT, {**query, "timeout_ms": value})
        self.rejected("InvalidArgument", QUIESCENCE, {**query, "limit": 65})
        self.rejected("InvalidArgument", WAIT_QUIESCENT, {**query, "timeout_ms": 50, "extra": True})
        self.rejected("Conflict", QUIESCENCE, initial)

        observed = n.call(QUIESCENCE, query)
        self.assertIs(observed["quiescent"], False, observed)
        self.assertEqual(observed["blockers"]["reference"], draining["reference"])
        page = observed["blockers"]["tasks"]
        self.assertTrue(page["truncated"], page)
        self.assertEqual(len(page["items"]), 1, page)
        before = monotonic()
        timed_out = n.call(WAIT_QUIESCENT, {**query, "timeout_ms": 160})
        self.assertGreaterEqual(monotonic() - before, 0.12)
        self.assertIs(timed_out["quiescent"], False, timed_out)
        self.assertEqual(timed_out["blockers"]["tasks"], page)

        self.restart()
        self.assertEqual(n.call(QUIESCENCE, query)["blockers"]["tasks"], page)
        with ThreadPoolExecutor(max_workers=1) as waiters:
            waiting = waiters.submit(n.call, WAIT_QUIESCENT, {**query, "timeout_ms": 5000})
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.15)
            n.call("Task.Cancel", target(n.get(tasks[0])["reference"]))
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.15)
            n.call("Task.Cancel", target(n.get(tasks[1])["reference"]))
            ready = waiting.result(timeout=3)
        self.assertIs(ready["quiescent"], True, ready)
        self.assertEqual(ready["blockers"]["reference"], draining["reference"])
        self.assertEqual(n.call("Node.Get", {})["state"]["scheduling"], "Cordoned")
        for task in tasks:
            self.assertEqual(n.wait(task)["lifecycle"], "Cancelled")
        for arguments in ((), ("--wait-ms", "1000")):
            cli = n.cli(n.witness / "owner.token", "node-work-admission-quiescence", revision, *arguments)
            self.assertIs(cli["quiescent"], True, cli)
            self.assertEqual(cli["blockers"]["reference"], draining["reference"])
        self.verify()

    def test_quiescence_wait_fails_when_admission_reopens(self):
        n = self.node
        self.scheduling("Cordoned")
        queued = n.submit("system.status", {})
        draining = self.change(self.admission(), "Draining", "hold initial maintenance revision")
        request = {"expected_resource_version": draining["reference"]["version"], "timeout_ms": 5000}
        with ThreadPoolExecutor(max_workers=1) as waiters:
            waiting = waiters.submit(n.call, WAIT_QUIESCENT, request)
            with self.assertRaises(TimeoutError):
                waiting.result(timeout=0.15)
            reopened = self.change(draining, "Open", "abort maintenance")
            with self.assertRaises(Failure) as conflict:
                waiting.result(timeout=3)
        self.assertEqual(conflict.exception.code, "Conflict", conflict.exception)
        self.assertEqual(n.call("Task.Get", {"id": queued})["lifecycle"], "Queued")
        revised = {"expected_resource_version": reopened["reference"]["version"]}
        self.rejected("PreconditionFailed", QUIESCENCE, revised)
        self.verify()

    def test_ordinary_submissions_pause_while_existing_tasks_and_approval_remain_valid(self):
        n = self.node
        outsider, _ = n.agent(["task.read"])
        initial = self.admission()
        self.assertEqual(initial["admission"]["mode"], "Open", initial)
        self.rejected("PermissionDenied", GET, {}, outsider["token"])
        self.rejected("PermissionDenied", SET, {
            "expected_resource_version": initial["reference"]["version"], "mode": "Draining",
            "reason": "non-owner cannot hold admissions",
        }, outsider["token"])
        self.rejected("InvalidArgument", SET, {
            "expected_resource_version": initial["reference"]["version"], "mode": "Draining", "reason": "\n",
        })
        self.assertEqual(self.admission(), initial)

        prior = n.perform("system.status", {})
        volume = n.perform("volume.create", {"name": "work-admission-test"})
        file = n.state / "volumes" / volume["resource_id"] / "data" / "admitted"
        self.scheduling("Cordoned")
        cancelled_parent = n.submit("system.status", {})
        n.call("Task.Cancel", target(n.get(cancelled_parent)["reference"]))
        parent = n.wait(cancelled_parent)
        self.assertEqual(parent["lifecycle"], "Cancelled", parent)
        retry_target = target(n.get(cancelled_parent)["reference"])
        existing_retry_request = {**retry_target, "key": key()}
        existing_retry = n.call("Task.Retry", existing_retry_request)
        fresh_retry = {**retry_target, "key": key()}

        original_request = n.prepare("system.status", {})
        original = n.call("Task.Submit", original_request)
        cancellable = n.submit("system.status", {})
        waiting = n.submit("volume.write", {**target(volume), "name": file.name, "content": "one approved effect"})
        waiting_task = n.wait(waiting)
        self.assertEqual(waiting_task["lifecycle"], "WaitingApproval", waiting_task)
        approval = n.get(waiting_task["conditions"][0]["message"])
        approve = {**target(approval["reference"]), "plan_digest": approval["value"]["plan_digest"], "approve": True}
        fresh = n.prepare("system.status", {})
        self.assertFalse(file.exists())

        opening = self.admission()
        rendezvous = Barrier(2)

        def enter(reason):
            rendezvous.wait(timeout=10)
            try:
                return self.change(opening, "Draining", reason)
            except Failure as error:
                return error.code

        reasons = ("prepare local maintenance", "hold new work during service maintenance")
        with ThreadPoolExecutor(max_workers=2) as readers:
            outcomes = list(readers.map(enter, reasons))
        self.assertEqual([outcome for outcome in outcomes if isinstance(outcome, str)], ["Conflict"], outcomes)
        draining, = [outcome for outcome in outcomes if isinstance(outcome, dict)]
        self.assertEqual(draining["admission"]["mode"], "Draining", draining)
        self.assertIn(draining["admission"]["reason"], reasons, draining)
        self.assertEqual(draining["admission"]["changed_by"], "local-owner", draining)
        self.assertEqual(draining["task_counts"], {
            "phases": {"Queued": 3, "WaitingApproval": 1}, "recovery_admissions": 0,
        }, draining)
        self.assertEqual(self.change(draining, "Draining", "repeat the same mode"), draining)
        self.assertEqual(n.cli(n.witness / "owner.token", "node-work-admission"), draining)
        self.assertEqual(n.call("Node.Get", {})["state"]["scheduling"], "Cordoned")
        self.rejected("Conflict", SET, {
            "expected_resource_version": opening["reference"]["version"], "mode": "Open", "reason": "stale",
        })

        self.restart()
        self.assertEqual(self.admission(), draining)
        self.assertEqual(n.call("Node.Get", {})["state"]["scheduling"], "Cordoned")
        before = self.inventory("Task", "Run", "Operation", "ApprovalRequest", "ApprovalDecision", "ApprovalUse")
        budget = {"id": n.get("owner-delegation")["value"]["budget_ref"]["resource_id"]}
        charge = n.call("Budget.Get", budget)
        self.assertEqual(n.call("Task.Submit", original_request), {**original, "deduplicated": True})
        self.assertEqual(n.call("Task.Retry", existing_retry_request), {**existing_retry, "deduplicated": True})
        self.blocked("Task.Submit", fresh)
        self.blocked("Task.Retry", fresh_retry)
        self.assertEqual(self.inventory(*before), before)
        self.assertEqual(n.call("Budget.Get", budget), charge)
        self.assertEqual(self.admission(), draining)

        known = n.call("Task.Get", {"id": original["task_id"]})
        self.assertEqual(known["lifecycle"], "Queued", known)
        self.assertEqual(n.call("Task.Wait", {"id": original["task_id"], "timeout_ms": 25}), known)
        self.assertTrue(n.get("profile-local")["reference"])
        n.call("Task.Cancel", target(n.get(cancellable)["reference"]))
        self.assertEqual(n.wait(cancellable)["lifecycle"], "Cancelled")
        n.call("Approval.Decide", approve)
        self.assertFalse(file.exists())
        self.scheduling("Enabled")
        for receipt in (original, existing_retry):
            result = self.task_result(receipt["task_id"])
            self.assertEqual(result["authority_id"], prior["authority_id"], result)
        self.task_result(waiting)
        self.assertEqual(file.read_text(), "one approved effect")
        finished = self.admission()
        self.assertEqual(finished["reference"], draining["reference"])
        self.assertEqual(finished["task_counts"], {"phases": {}, "recovery_admissions": 0}, finished)
        self.blocked("Task.Submit", fresh)
        self.blocked("Task.Retry", fresh_retry)

        # Opening admission under Cordon accepts work but leaves actual dispatch paused.
        self.scheduling("Cordoned")
        reopened = n.cli(n.witness / "owner.token", "node-work-admission-set", draining["reference"]["version"],
                         "open", "maintenance complete")
        self.assertEqual(reopened["admission"]["mode"], "Open", reopened)
        self.assertEqual(n.call("Node.Get", {})["state"]["scheduling"], "Cordoned")
        accepted = n.call("Task.Submit", fresh)["task_id"]
        retried = n.call("Task.Retry", fresh_retry)["task_id"]
        for task_id in (accepted, retried):
            self.assertEqual(n.call("Task.Get", {"id": task_id})["lifecycle"], "Queued")
        self.scheduling("Enabled")
        for task_id in (accepted, retried):
            self.assertEqual(self.task_result(task_id)["authority_id"], prior["authority_id"])
        self.assertEqual(n.call("Task.Get", {"id": retried})["retry_of_task_id"], cancelled_parent)
        self.assertEqual(file.read_text(), "one approved effect")
        self.verify()

    def test_existing_model_leases_settle_one_real_tool_while_new_claims_wait(self):
        n = self.node
        provider = n.call("Agent.Register", {"name": "drain-model-provider"})
        agent = n.call("Agent.Register", {"name": "drain-session-owner"})
        model = n.call("Model.Register", {
            "name": "drain-contract-model", "revision": "1", "provider_principal_id": provider["principal_id"],
            "inference_outcome_policy": "Strict", "lease_seconds": 45, "max_in_flight": 3,
            "max_context_bytes": 32768, "max_output_bytes": 8192,
        })
        n.call("Grant.Issue", {
            "principal_id": provider["principal_id"], "actions": ["model.serve"],
            "tool_targets": [model["resource_id"]], "units": 8, "ttl_seconds": 240,
        })
        skill = publish_skill(n)
        grant = n.call("Grant.Issue", {
            "principal_id": agent["principal_id"],
            "actions": ["session.run", "task.submit", "task.read", "system.status"],
            "tool_targets": [model["resource_id"], skill["resource_id"], "profile-local"],
            "units": 8, "ttl_seconds": 240, "model_calls": 8, "model_lease_ms": 360_000,
        })

        def open_session(queued):
            return n.call("Session.Open", {
                "key": key(), "provider_queue": queued, "goal": "Read the current system status.",
                "skill_ref": skill, "model_ref": model, "context_policy": "LocalToolResults",
                "tool_context_window": 2, "max_turns": 4,
                "delegation_id": grant["delegation_ref"]["resource_id"],
            }, agent["token"])["reference"]["resource_id"]

        def provider_call(method, request):
            return n.call(method, request, provider["token"])

        def decision(session_id, claim, value):
            return {"session_id": session_id, "lease_id": claim["lease_id"], "decision": value}

        def charge():
            budget = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
            return budget["model_calls_used"], budget["spent"]

        direct_session = open_session(False)
        queued_session = open_session(True)
        direct_request = {"session_id": direct_session, "key": key()}
        queue_request = {"model_ref": model, "key": key()}
        direct = provider_call("Model.Claim", direct_request)
        queued = provider_call("Model.ClaimNext", queue_request)
        self.assertEqual(direct["turn"], 1, direct)
        self.assertEqual(queued["session_id"], queued_session)
        self.assertEqual(charge(), (2, 0))

        waiting_direct = {"session_id": open_session(False), "key": key()}
        waiting_session = open_session(True)
        waiting_queue = {"model_ref": model, "key": key()}
        draining = self.change(self.admission(), "Draining", "allow existing inference to settle")
        self.assertEqual(draining["model_counts"], {"current_leases": 2, "paused_outcomes_unknown": 0}, draining)
        self.restart()
        self.assertEqual(self.admission()["reference"], draining["reference"])
        for method, request, claim in (("Model.Claim", direct_request, direct),
                                       ("Model.ClaimNext", queue_request, queued)):
            replayed = provider_call(method, request)
            self.assertEqual((replayed.get("session_id"), replayed["turn"], replayed["lease_id"]),
                             (claim.get("session_id"), claim["turn"], claim["lease_id"]), replayed)
        self.blocked("Model.Claim", waiting_direct, provider["token"])
        self.assertEqual(provider_call("Model.ClaimNext", waiting_queue), {"available": False})
        self.assertEqual(provider_call("Model.WaitNext", {**waiting_queue, "timeout_ms": 40}), {"available": False})
        self.assertEqual(provider_call("Model.ClaimStatus", {**queue_request, "lease_id": queued["lease_id"]}),
                         {"status": "Pending"})
        self.assertEqual(charge(), (2, 0))
        self.assertEqual(self.inventory("Task")["Task"], set())

        response = decision(direct_session, direct, STATUS)
        accepted = provider_call("Model.Respond", response)
        task_id = accepted["task_id"]
        result = self.task_result(task_id, agent["token"])
        self.assertEqual(result["authority_id"], n.call("Principal.WhoAmI", {})["authority_id"], result)
        self.assertTrue(result["kernel_release"], result)
        self.assertEqual(provider_call("Model.Respond", response), accepted)
        self.assertEqual(provider_call("Model.Claim", direct_request)["completed"], accepted)
        queued_finish = provider_call("Model.Respond", decision(queued_session, queued, {
            "action": "finish", "summary": "Completed the already leased queued turn.",
        }))
        self.assertEqual(queued_finish["lifecycle"], "Succeeded", queued_finish)
        self.assertEqual(provider_call("Model.ClaimNext", queue_request)["completed"], queued_finish)
        self.assertEqual(provider_call("Model.ClaimStatus", {**queue_request, "lease_id": queued["lease_id"]}),
                         {"status": "Settled"})
        history = n.call("Session.History", {"id": direct_session}, agent["token"])
        self.assertEqual(history["tasks"], [{"task_id": task_id, "turn": 1, "attempt": 1}], history)
        self.assertEqual(history["turns"][0]["decision"], accepted, history)
        self.assertEqual(self.inventory("Task")["Task"], {task_id})
        self.assertEqual(charge(), (2, 1))
        self.assertEqual(self.admission()["model_counts"], {"current_leases": 0, "paused_outcomes_unknown": 0})
        self.blocked("Model.Claim", {"session_id": direct_session, "key": key()}, provider["token"])
        self.assertEqual(provider_call("Model.ClaimNext", waiting_queue), {"available": False})

        self.change(self.admission(), "Open", "resume new inference leases")
        fresh_direct = provider_call("Model.Claim", waiting_direct)
        fresh_queue = provider_call("Model.ClaimNext", waiting_queue)
        self.assertEqual(fresh_direct["turn"], 1, fresh_direct)
        self.assertNotEqual(fresh_direct["lease_id"], direct["lease_id"])
        self.assertEqual(fresh_queue["session_id"], waiting_session)
        self.assertEqual(charge(), (4, 1))
        for session_id, claim in ((waiting_direct["session_id"], fresh_direct), (waiting_session, fresh_queue)):
            final = provider_call("Model.Respond", decision(session_id, claim, {
                "action": "finish", "summary": "Completed after reopening admission.",
            }))
            self.assertEqual(final["lifecycle"], "Succeeded", final)
        self.assertEqual(self.inventory("Task")["Task"], {task_id})
        self.verify()

    def test_exact_storage_recovery_uses_its_reserve_even_with_free_ordinary_capacity(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "drain-storage-recovery"})
        original_data = n.state / "volumes" / volume["resource_id"] / "data" / "record"
        written = b"exact storage result remains after a lost restore acknowledgement\n"
        original_data.write_bytes(written)
        backup = n.perform("backup.create", target(volume))
        original, unresolved = lose_restore_receipt(n, backup)
        self.assertEqual((unresolved["lifecycle"], unresolved["outcome_knowledge"]), ("Paused", "Unknown"), unresolved)
        operation, = unresolved["operation_refs"]
        restored_id = f"volume-{operation['resource_id']}"
        restored = n.state / "volumes" / restored_id / "data" / "record"
        original_inode = restored.stat().st_ino
        self.assertEqual(restored.read_bytes(), written)

        tool = "volume.inspect_publication"
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", tool],
            "tool_targets": [operation["resource_id"]], "units": 4, "ttl_seconds": 120,
        })
        request = n.prepare(tool, target(operation), delegation=grant["delegation_ref"]["resource_id"])
        self.scheduling("Cordoned")
        draining = self.change(self.admission(), "Draining", "continue only exact recovery proof")
        self.assertEqual(draining["task_counts"], {"phases": {"Unknown": 1}, "recovery_admissions": 0}, draining)
        prior = n.call("Node.Get", {})["admission"]
        self.assertLess(prior["ordinary"]["used"], prior["ordinary"]["limit"], prior)
        self.blocked("Task.Submit", n.prepare("system.status", {}))
        self.rejected("PermissionDenied", "Task.Prepare", {
            "tool": tool, "input": target(operation), "key": key(), "delegation_id": "owner-delegation",
        })

        inspection = n.call("Task.Submit", request)
        task = inspection["task_id"]
        admitted = n.get(f"recovery-admission-{task}")["value"]
        self.assertEqual(admitted["unresolved_operation_ref"], operation, admitted)
        view = n.call("Node.Get", {})["admission"]
        self.assertEqual(view["ordinary"], prior["ordinary"], view)
        self.assertEqual(view["recovery"]["used"], prior["recovery"]["used"] + 1, view)
        self.assertEqual(self.admission()["task_counts"]["recovery_admissions"], 1)
        self.restart()
        self.assertEqual(n.call("Task.Submit", request), {**inspection, "deduplicated": True})
        self.assertEqual(self.admission()["task_counts"]["recovery_admissions"], 1)
        self.scheduling("Enabled")
        evidence_task = n.wait(task)
        self.assertEqual(evidence_task["lifecycle"], "Succeeded", evidence_task)
        evidence, = evidence_task["result"]["artifact_refs"]
        receipt = n.call("Operation.Reconcile", {**target(operation), "evidence_ref": evidence})
        self.assertEqual(receipt["outcome_knowledge"], "Known", receipt)
        self.assertEqual(n.wait(original)["lifecycle"], "Succeeded")
        self.assertEqual(restored.stat().st_ino, original_inode)
        self.assertEqual(restored.read_bytes(), written)
        final = self.admission()
        self.assertEqual(final["reference"], draining["reference"], final)
        self.assertEqual(final["task_counts"], {"phases": {}, "recovery_admissions": 0}, final)
        self.verify()

    def test_due_schedule_waits_and_dispatches_once_when_opened(self):
        n = self.node
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["schedule.manage", "task.submit", "task.read", "system.status"],
            "tool_targets": ["profile-local"], "units": 4, "ttl_seconds": 90,
        })
        self.scheduling("Cordoned")
        due = timestamp(1)
        request = {
            "key": key(), "tool": "system.status", "input": {}, "run_at": due, "deadline_at": timestamp(25),
            "delegation_id": grant["delegation_ref"]["resource_id"],
        }
        scheduled = n.call("Schedule.Create", request)
        self.assertEqual(scheduled["state"]["lifecycle"], "Pending", scheduled)
        draining = self.change(self.admission(), "Draining", "wait for unrelated scheduled submissions")
        self.scheduling("Enabled")
        after(due, grace=0.3)
        budget = {"id": grant["budget_ref"]["resource_id"]}
        charge = n.call("Budget.Get", budget)
        self.assertEqual((charge["reserved"], charge["spent"]), (0, 0), charge)
        self.assertEqual(self.inventory("Task")["Task"], set())
        scope = {"id": scheduled["reference"]["resource_id"]}
        self.assertEqual(n.call("Schedule.Get", scope), scheduled)

        self.restart()
        self.assertEqual(self.admission()["reference"], draining["reference"])
        self.assertEqual(n.call("Schedule.Get", scope), scheduled)
        self.change(self.admission(), "Open", "permit the existing due schedule")
        deadline = monotonic() + 8
        while True:
            current = n.call("Schedule.Get", scope)
            if current["state"]["lifecycle"] != "Pending":
                break
            self.assertLess(monotonic(), deadline, current)
            sleep(0.035)
        self.assertEqual(current["state"]["lifecycle"], "Triggered", current)
        task = current["state"]["task_receipt"]["task_id"]
        self.assertTrue(self.task_result(task)["authority_id"])
        self.assertEqual(self.inventory("Task")["Task"], {task})
        self.assertEqual(n.call("Schedule.Create", request), current)
        charge = n.call("Budget.Get", budget)
        self.assertEqual((charge["reserved"], charge["spent"]), (0, 1), charge)
        self.verify()

    def test_completed_workflow_step_releases_no_successor_until_opened(self):
        from test_workflow import step

        n = self.node
        volume = n.perform("volume.create", {"name": "workflow-drain-boundary"})
        path = n.state / "volumes" / volume["resource_id"] / "data" / "approved"
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["workflow.manage", "task.submit", "task.read",
                                                      "volume.write", "system.status"],
            "tool_targets": [volume["resource_id"], "profile-local"], "units": 2, "ttl_seconds": 120,
        })
        definition = n.call("Workflow.Define", {"key": key(), "steps": [
            step("write", "volume.write", {**target(volume), "name": path.name, "content": "approved before drain"}),
            step("observe", after=["write"]),
        ]})
        request = {"key": key(), "definition_ref": definition["reference"], "deadline_at": timestamp(45),
                   "delegation_id": grant["delegation_ref"]["resource_id"]}
        baseline = self.inventory("Task")["Task"]
        run = n.call("Workflow.Start", request)
        run_id = run["reference"]["resource_id"]
        scope = {"id": run_id}
        budget = {"id": grant["budget_ref"]["resource_id"]}

        def workflow_at(lifecycle):
            deadline = monotonic() + 8
            while True:
                current = n.call("Workflow.Get", scope)
                phase = current["state"]["lifecycle"]
                if phase == lifecycle:
                    return current, {entry["step_id"]: entry for entry in current["state"]["steps"]}
                self.assertNotIn(phase, ("Succeeded", "Failed", "Cancelled"), current)
                self.assertLess(monotonic(), deadline, current)
                sleep(0.035)

        _, pending = workflow_at("WaitingApproval")
        first = pending["write"]["task_receipt"]["task_id"]
        self.assertIsNone(pending["observe"]["task_receipt"], pending)
        self.assertEqual(self.inventory("Task")["Task"], baseline | {first})
        self.assertFalse(path.exists())
        draining = self.change(self.admission(), "Draining", "hold dependent workflow dispatch")
        n.approve(first)
        self.task_result(first)
        deferred, steps = workflow_at("Ready")
        self.assertEqual(steps["write"]["lifecycle"], "Succeeded", steps)
        self.assertEqual(steps["write"]["task_ref"], n.get(first)["reference"], steps)
        self.assertEqual(steps["observe"]["lifecycle"], "Pending", steps)
        self.assertIsNone(steps["observe"]["task_receipt"], steps)
        self.assertEqual(path.read_text(), "approved before drain")
        written = path.stat()
        charge = n.call("Budget.Get", budget)
        self.assertEqual((charge["reserved"], charge["spent"]), (0, 1), charge)
        self.assertEqual(self.inventory("Task")["Task"], baseline | {first})

        self.restart()
        self.assertEqual(self.admission()["reference"], draining["reference"])
        self.assertEqual(n.call("Workflow.Start", request), deferred)
        self.assertEqual(self.inventory("Task")["Task"], baseline | {first})
        self.change(draining, "Open", "resume the dependent workflow step")
        finished, completed = workflow_at("Succeeded")
        second_receipt = completed["observe"]["task_receipt"]
        second = second_receipt["task_id"]
        self.assertEqual(second_receipt["origin_ref"]["origin_type"], "WorkflowRun", second_receipt)
        self.assertEqual(second_receipt["origin_ref"]["origin_id"], run_id, second_receipt)
        self.assertEqual(second_receipt["origin_ref"]["step_key"], completed["observe"]["step_key"], second_receipt)
        self.assertEqual(completed["write"]["task_ref"], steps["write"]["task_ref"])
        self.assertEqual(completed["observe"]["task_ref"], n.get(second)["reference"])
        self.assertTrue(self.task_result(second)["authority_id"])
        self.assertEqual(n.call("Workflow.Start", request), finished)
        self.assertEqual(self.inventory("Task")["Task"], baseline | {first, second})
        charge = n.call("Budget.Get", budget)
        self.assertEqual((charge["reserved"], charge["spent"]), (0, 2), charge)
        settled = path.stat()
        self.assertEqual((settled.st_ino, settled.st_mtime_ns), (written.st_ino, written.st_mtime_ns))
        self.verify()


if __name__ == "__main__":
    unittest.main()
