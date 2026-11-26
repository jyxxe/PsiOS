"""Run literal Task DAGs through the real authority, approval and crash recovery."""
import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from datetime import datetime
import hmac
import json
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from test_schedule import timestamp


def step(name, tool="system.status", input=None, after=(), when=None, after_selected=()):
    definition = dict(id=name, tool=tool, input={} if input is None else input, after=list(after))
    if when is not None:
        definition["when"] = when
    if after_selected:
        definition["after_selected"] = list(after_selected)
    return definition


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)

    def grant(self, actions=("system.status",), targets=("profile-local",), units=3, actor="local-owner"):
        return self.node.call("Grant.Issue", dict(principal_id=actor, tool_targets=list(targets), units=units,
            actions=["workflow.manage", "task.submit", "task.read", *actions], ttl_seconds=120))

    def define(self, steps, token=None, *, max_parallel_steps=None):
        request = {"key": key(), "steps": steps}
        if max_parallel_steps is not None:
            request["max_parallel_steps"] = max_parallel_steps
        return self.node.call("Workflow.Define", request, token)

    def request(self, definition, grant, deadline=30):
        return dict(key=key(), definition_ref=definition["reference"], deadline_at=timestamp(deadline),
                    delegation_id=grant["delegation_ref"]["resource_id"])

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as reason:
            self.node.call(method, params, token)
        self.assertEqual(reason.exception.code, code, reason.exception)

    def budget(self, grant):
        view = self.node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        return view["reserved"], view["spent"]

    def fetch(self, instance, token=None):
        return self.node.call("Workflow.Get", {"id": instance["reference"]["resource_id"]}, token)

    def wait(self, instance, phase):
        deadline = time.monotonic() + 10
        while True:
            current = self.fetch(instance)
            if current["state"]["lifecycle"] == phase:
                return current
            self.assertNotIn(current["state"]["lifecycle"], ["Succeeded", "Failed", "Cancelled"], current)
            self.assertLess(time.monotonic(), deadline, current)
            time.sleep(0.05)

    def node_scheduling(self, scheduling):
        reference = self.node.call("Node.Get", {})["reference"]
        self.node.call("Node.Configure", dict(scheduling=scheduling, expected_resource_version=reference["version"]))

    def phases(self, current):
        return {step["step_id"]: step for step in current["state"]["steps"]}

    def wait_active(self, instance, steps):
        deadline = time.monotonic() + 10
        while True:
            current = self.fetch(instance)
            if current["state"]["active_steps"] == steps:
                return current
            self.assertNotIn(current["state"]["lifecycle"], ["Succeeded", "Failed", "Cancelled"], current)
            self.assertLess(time.monotonic(), deadline, current)
            time.sleep(0.05)

    def test_bounded_parallel_approvals_survive_a_crash_and_join_once(self):
        n = self.node
        volumes = [n.perform("volume.create", {"name": f"parallel-{name}"}) for name in "abc"]
        scope = self.grant(["volume.write", "system.status"],
                           ["profile-local", *(volume["resource_id"] for volume in volumes)], units=4)
        steps = [step(name, "volume.write", {**target(volume), "name": "receipt", "content": name})
                 for name, volume in zip("abc", volumes)] + [step("join", after=["a", "b", "c"])]
        params = {"key": key(), "steps": steps, "max_parallel_steps": 2}
        self.rejected("InvalidArgument", "Workflow.Define", dict(params, max_parallel_steps=0))
        definition = n.call("Workflow.Define", params)
        self.assertEqual(n.call("Workflow.Define", params), definition)
        self.rejected("IdempotencyConflict", "Workflow.Define", dict(params, max_parallel_steps=3))
        self.assertEqual(definition["definition"]["max_parallel_steps"], 2)
        run = n.call("Workflow.Start", self.request(definition, scope))
        active = self.wait_active(run, ["a", "b"])
        entries = self.phases(active)
        a, b = [entries[name]["task_receipt"]["task_id"] for name in "ab"]
        self.assertEqual(active["state"]["lifecycle"], "WaitingApproval")
        self.assertIsNone(entries["c"]["task_receipt"])
        self.assertIsNone(entries["join"]["task_receipt"])
        self.assertEqual(self.budget(scope), (2, 0))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.fetch(run), active)
        self.assertEqual(self.budget(scope), (2, 0))

        n.approve(b)
        next_pair = self.wait_active(run, ["a", "c"])
        projections = self.phases(next_pair)
        c = projections["c"]["task_receipt"]["task_id"]
        self.assertEqual(projections["b"]["task_receipt"]["task_id"], b)
        self.assertEqual(projections["b"]["lifecycle"], "Succeeded")
        self.assertIsNone(projections["join"]["task_receipt"])
        self.assertEqual(self.budget(scope), (2, 1))
        with ThreadPoolExecutor(max_workers=2) as callers:
            list(callers.map(n.approve, [a, c]))
        done = self.wait(run, "Succeeded")
        self.assertEqual(done["state"]["active_steps"], [])
        final = self.phases(done)
        self.assertEqual([final[name]["task_receipt"]["task_id"] for name in "abc"], [a, b, c])
        self.assertTrue(all(projection["lifecycle"] == "Succeeded" for projection in final.values()))
        self.assertEqual(self.budget(scope), (0, 4))
        for name, volume in zip("abc", volumes):
            content = n.state / "volumes" / volume["resource_id"] / "data" / "receipt"
            self.assertEqual(content.read_text(), name)

    def test_step_rejection_cancels_parallel_sibling_approvals(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "parallel-rejection"})
        scope = self.grant(["volume.write"], [volume["resource_id"]], units=3)
        definition = self.define([step(name, "volume.write", {**target(volume), "name": name, "content": name})
                                  for name in "abc"], max_parallel_steps=2)
        run = n.call("Workflow.Start", self.request(definition, scope))
        active = self.wait_active(run, ["a", "b"])
        entries = self.phases(active)
        first = entries["a"]["task_receipt"]["task_id"]
        sibling = entries["b"]["task_receipt"]["task_id"]
        n.approve(first, approve=False)
        stopped = self.wait(run, "Failed")
        self.assertEqual(stopped["state"]["active_steps"], [])
        self.assertEqual(n.wait(sibling)["lifecycle"], "Cancelled")
        self.assertIsNone(self.phases(stopped)["c"]["task_receipt"])
        self.assertEqual(self.budget(scope), (0, 0))
        self.assertEqual(list((n.state / "volumes" / volume["resource_id"] / "data").iterdir()), [])

    def test_unknown_parallel_child_fences_admission_while_a_sibling_settles(self):
        n = self.node
        two = [n.perform("volume.create", {"name": f"unknown-{name}"}) for name in "ab"]
        scope = self.grant(["volume.write", "system.status"],
                           ["profile-local", *(volume["resource_id"] for volume in two)], units=3)
        definition = self.define([
            step(name, "volume.write", {**target(volume), "name": "written", "content": name})
            for name, volume in zip("ab", two)
        ] + [step("c", after=["b"])], max_parallel_steps=2)
        run = n.call("Workflow.Start", self.request(definition, scope))
        entries = self.phases(self.wait_active(run, ["a", "b"]))
        task, sibling = [entries[name]["task_receipt"]["task_id"] for name in "ab"]
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute(f"""CREATE TRIGGER fail_one_branch BEFORE INSERT ON artifact_chunks
                WHEN EXISTS(SELECT 1 FROM versions v WHERE v.id=NEW.artifact_id AND v.spec->>'task_id'='{task}')
                BEGIN SELECT RAISE(ABORT,'test parallel receipt loss'); END""")
        n.start()
        n.approve(task)
        uncertain = self.wait(run, "Blocked")
        self.assertEqual(uncertain["state"]["reason"], "OutcomeUnknown")
        effect = n.state / "volumes" / two[0]["resource_id"] / "data" / "written"
        before = effect.stat()
        self.assertEqual(effect.read_text(), "a")
        n.approve(sibling)
        partial = self.wait_active(run, ["a"])
        self.assertEqual(partial["state"]["reason"], "OutcomeUnknown")
        self.assertEqual(self.phases(partial)["b"]["lifecycle"], "Succeeded")
        self.assertIsNone(self.phases(partial)["c"]["task_receipt"])

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER fail_one_branch")
        n.start()
        self.assertEqual(self.fetch(run), partial)
        old_operation, = n.wait(task)["operation_refs"]
        n.call("Operation.Reconcile", target(old_operation))
        done = self.wait(run, "Succeeded")
        self.assertEqual(self.phases(done)["a"]["task_receipt"]["task_id"], task)
        self.assertEqual(self.phases(done)["b"]["task_receipt"]["task_id"], sibling)
        self.assertEqual(self.budget(scope), (0, 3))
        after = effect.stat()
        self.assertEqual((after.st_ino, after.st_mtime_ns), (before.st_ino, before.st_mtime_ns))

    def test_artifact_mapping_requires_read_scope_and_rejects_invalid_inputs(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "binding-fences"})
        targets = ["profile-local", volume["resource_id"]]
        scope = self.grant(["system.status", "volume.write"], targets, units=4)
        prepare = step("source")
        base = step("write", "volume.write", dict(**target(volume), name="never", content="seed"), after=["source"])
        text = {"step": "source", "pointer": "/authority_id"}
        for invalid in (dict(base, inputs_from={"id": text}),
                        dict(base, after=[], after_selected=["source"], inputs_from={"content": text}),
                        dict(base, inputs_from={"content": dict(text, pointer="/~2bad")})):
            self.rejected("InvalidArgument", "Workflow.Define", {"key": key(), "steps": [prepare, invalid]})

        valid = self.define([prepare, dict(base, inputs_from={"content": text})])
        no_read = n.call("Grant.Issue", {"principal_id": "local-owner", "units": 2, "ttl_seconds": 120,
            "actions": ["workflow.manage", "task.submit", "system.status", "volume.write"], "tool_targets": targets})
        self.rejected("PermissionDenied", "Workflow.Start", self.request(valid, no_read))

        for pointer in ("/absent", ""):
            with self.subTest(pointer=pointer):
                definition = self.define([prepare,
                    dict(base, inputs_from={"content": dict(text, pointer=pointer)})])
                run = n.call("Workflow.Start", self.request(definition, scope))
                failed = self.wait(run, "Failed")
                projection = self.phases(failed)["write"]
                self.assertEqual(projection["reason"], "PreconditionFailed")
                self.assertIsNone(projection["task_receipt"])
                self.assertNotIn("input_binding", projection)
        self.assertEqual(self.budget(scope), (0, 2))
        self.assertFalse((n.state / "volumes" / volume["resource_id"] / "data" / "never").exists())

    def test_artifact_mapping_can_feed_a_scoped_mac(self):
        n = self.node
        key_bytes = b"workflow-testing-key"
        secret = n.call("Secret.Put", {"name": "mapped-key", "value": key_bytes.decode()})
        scope = self.grant(["secret.mac", "system.status"], [secret["resource_id"], "profile-local"], units=2)
        mac = step("seal", "secret.mac", {"secret_id": secret["resource_id"], "message": "seed"}, after=["source"])
        mac["inputs_from"] = {"message": {"step": "source", "pointer": "/authority_id"}}
        definition = self.define([step("source"), mac])
        run = n.call("Workflow.Start", self.request(definition, scope))
        finished = self.wait(run, "Succeeded")
        task = self.phases(finished)["seal"]["task_receipt"]["task_id"]
        reference, = n.wait(task)["result"]["artifact_refs"]
        tag = n.call("Artifact.Read", {"reference": reference})["tag"]
        authority = n.call("Principal.WhoAmI", {})["authority_id"]
        self.assertEqual(base64.b64decode(tag), hmac.digest(key_bytes, authority.encode(), "sha256"))
        self.assertEqual(self.budget(scope), (0, 2))

    def test_active_workflow_retains_its_predecessor_until_completion(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "retained-input"})
        scope = self.grant(["system.status", "volume.write"], ["profile-local", volume["resource_id"]], units=2)
        write = step("write", "volume.write", {**target(volume), "name": "proof", "content": "template"},
                     after=["source"])
        write["inputs_from"] = {"content": {"step": "source", "pointer": "/authority_id"}}
        definition = self.define([step("source"), write])
        run = n.call("Workflow.Start", self.request(definition, scope))
        phases = self.phases(self.wait(run, "WaitingApproval"))
        source = phases["source"]["task_ref"]
        request = {**target(source), "reason": "old observation"}
        self.rejected("Conflict", "Task.EvictResults", request)
        n.stop(signal.SIGKILL)
        n.start()
        self.rejected("Conflict", "Task.EvictResults", request)
        write_task = phases["write"]["task_receipt"]["task_id"]
        n.approve(write_task)
        final = self.wait(run, "Succeeded")
        self.assertEqual(self.budget(scope), (0, 2))
        path = n.state / "volumes" / volume["resource_id"] / "data" / "proof"
        self.assertEqual(path.read_text(), n.call("Principal.WhoAmI", {})["authority_id"])
        self.assertEqual(n.call("Task.EvictResults", request)["eviction"]["previous_task_ref"], source)
        self.assertEqual(self.fetch(run), final)

    def test_resource_event_starts_one_workflow_with_shared_budget_and_approval(self):
        n = self.node
        self.grant()
        volume = n.perform("volume.create", {"name": "brokered-workflow"})
        write = step("record", "volume.write", {**target(volume), "name": "proof", "content": "scheduled"},
                     after=["observe"])
        definition = self.define([step("observe"), write])["reference"]
        scope = self.grant([
            "workflow.start", "system.status", "volume.write", "schedule.manage", "resource.read", "event.read",
        ], [definition["resource_id"], "profile-local", volume["resource_id"]], units=3)
        due = timestamp(35)
        request = {
            "key": key(), "tool": "workflow.start",
            "input": {"definition_ref": definition, "max_runtime_seconds": 600},
            "delegation_id": scope["delegation_ref"]["resource_id"],
            "event": {"source_ref": n.call("Node.Get", {})["reference"],
                      "condition": {"kind": "SpecChanged"}, "expires_at": due, "max_triggers": 1},
        }
        planned = n.call("Schedule.Create", request)
        self.node_scheduling("Cordoned")
        self.node_scheduling("Enabled")
        deadline = time.monotonic() + 8
        while True:
            schedule = n.call("Schedule.Get", {"id": planned["reference"]["resource_id"]})
            if schedule["state"]["lifecycle"] == "Completed":
                break
            self.assertLess(time.monotonic(), deadline, schedule)
            time.sleep(.05)
        trigger_id = schedule["state"]["trigger_refs"][0]["resource_id"]
        trigger = n.call("Schedule.TriggerGet", {"id": trigger_id})["state"]
        task = n.wait(trigger["task_receipt"]["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded")
        launch = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        self.assertEqual(launch["definition_ref"], definition)
        self.assertEqual(datetime.fromisoformat(launch["deadline_at"]), datetime.fromisoformat(due))
        parent = n.call("Resource.Resolve", {"reference": launch["origin_task_ref"]})["value"]
        self.assertEqual(parent["task_id"], task["task_id"])
        self.assertEqual(parent["origin_ref"]["origin_id"], planned["reference"]["resource_id"])
        run = {"reference": launch["workflow_run_ref"]}
        awaiting = self.wait(run, "WaitingApproval")
        self.assertEqual(awaiting["state"]["origin_task_ref"], launch["origin_task_ref"])
        self.assertEqual(self.budget(scope), (1, 2))
        with closing(sqlite3.connect(n.state / "state.db")) as db:
            parent_cutoff, = db.execute("SELECT deadline_boot_ms FROM authorization_deadlines WHERE resource=?",
                                       (task["task_id"],)).fetchone()
            run_cutoff, = db.execute("SELECT deadline_boot_ms FROM authorization_deadlines WHERE resource=?",
                                    (run["reference"]["resource_id"],)).fetchone()
        self.assertLessEqual(run_cutoff, parent_cutoff)
        path = n.state / "volumes" / volume["resource_id"] / "data" / "proof"
        self.assertFalse(path.exists())

        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), schedule)
        self.assertEqual(n.call("Schedule.TriggerGet", {"id": trigger_id})["state"], trigger)
        self.assertEqual(self.fetch(run), awaiting)
        self.assertEqual(len(n.call("Resource.List", {"kind": "WorkflowRun"})["items"]), 1)
        n.approve(self.phases(awaiting)["record"]["task_receipt"]["task_id"])
        self.wait(run, "Succeeded")
        self.assertEqual(path.read_text(), "scheduled")
        self.assertEqual(self.budget(scope), (0, 3))

    def test_brokered_workflow_requires_explicit_scope_and_disallows_recursive_steps(self):
        n = self.node
        self.grant()
        definition = self.define([step("observe")])["reference"]
        blueprint = {"definition_ref": definition, "max_runtime_seconds": 30}
        self.rejected("UnsupportedCapability", "Workflow.Define",
                      {"key": key(), "steps": [step("recursive", "workflow.start", blueprint)]})
        self.rejected("PermissionDenied", "Task.Prepare", {
            "tool": "workflow.start", "input": blueprint, "key": key(), "delegation_id": "owner-delegation",
        })
        untargeted = self.grant(["workflow.start", "system.status"], ["profile-local"], units=2)
        prepared = n.prepare("workflow.start", blueprint, delegation=untargeted["delegation_ref"]["resource_id"])
        self.rejected("PermissionDenied", "Task.Submit", prepared)
        incomplete = n.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 2, "ttl_seconds": 90,
            "tool_targets": [definition["resource_id"], "profile-local"],
            "actions": ["workflow.start", "system.status", "task.submit", "task.read"],
        })
        prepare = n.prepare("workflow.start", blueprint, delegation=incomplete["delegation_ref"]["resource_id"])
        task = n.wait(n.call("Task.Submit", prepare)["task_id"])
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertEqual(task["conditions"][0]["type"], "PermissionDenied")
        self.assertEqual(self.budget(incomplete), (0, 0))
        self.assertEqual(n.call("Resource.List", {"kind": "WorkflowRun"})["items"], [])

    def test_brokered_workflow_rolls_back_child_when_launch_result_cannot_commit(self):
        n = self.node
        self.grant()
        definition = self.define([step("observe")])["reference"]
        scope = self.grant(["workflow.start", "system.status"], [definition["resource_id"], "profile-local"], units=2)
        blueprint = {"definition_ref": definition, "max_runtime_seconds": 30}
        delegation = scope["delegation_ref"]["resource_id"]
        request = n.prepare("workflow.start", blueprint, delegation=delegation)
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("""
                CREATE TRIGGER fail_launch_receipt BEFORE INSERT ON artifact_chunks
                BEGIN SELECT RAISE(ABORT, 'test workflow launch receipt failure'); END
            """)
        n.start()
        failed = n.wait(n.call("Task.Submit", request)["task_id"])
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertEqual(failed["conditions"][0]["type"], "StorageUnavailable")
        self.assertEqual(n.call("Resource.List", {"kind": "WorkflowRun"})["items"], [])
        self.assertEqual(self.budget(scope), (0, 0))
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER fail_launch_receipt")
        n.start()
        self.assertTrue(n.call("Task.Submit", request)["deduplicated"])
        fresh = n.prepare("workflow.start", blueprint, delegation=delegation)
        task = n.wait(n.call("Task.Submit", fresh)["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded")
        result = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        self.wait({"reference": result["workflow_run_ref"]}, "Succeeded")
        self.assertEqual(self.budget(scope), (0, 2))

    def test_artifact_mapping_enforces_the_destination_tool_size_limit(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "oversize-binding"})
        n.perform("volume.write", dict(**target(volume), name="long", content="x" * 8193))
        fresh = n.get(volume["resource_id"])["reference"]
        secret = n.call("Secret.Put", {"name": "fixture", "value": "local-only"})
        scope = self.grant(["volume.read", "secret.mac"], [fresh["resource_id"], secret["resource_id"]], units=2)
        read = step("read", "volume.read", dict(**target(fresh), name="long"))
        mac = step("seal", "secret.mac", {"secret_id": secret["resource_id"], "message": ""}, after=["read"])
        mac["inputs_from"] = {"message": {"step": "read", "pointer": "/content"}}
        definition = self.define([read, mac])
        run = n.call("Workflow.Start", self.request(definition, scope))
        failed = self.wait(run, "Failed")
        blocked = self.phases(failed)["seal"]
        self.assertEqual(blocked["reason"], "QuotaExceeded")
        self.assertIsNone(blocked["task_receipt"])
        self.assertEqual(self.budget(scope), (0, 1))

    def test_join_resumes_original_approval_and_executes_each_step_once(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "workflow-join"})
        scope = self.grant(["system.status", "volume.write"], ["profile-local", volume["resource_id"]])
        definition = self.define([
            step("join", after=["prepare", "write"]),
            step("write", "volume.write", dict(**target(volume), name="proof", content="once"), after=["prepare"]),
            step("prepare"),
        ])
        self.assertEqual(n.call("Workflow.DefinitionGet", {"id": definition["reference"]["resource_id"]}), definition)
        request = self.request(definition, scope)
        self.rejected("PermissionDenied", "Workflow.Start", dict(request, delegation_id="owner-delegation"))
        with ThreadPoolExecutor(max_workers=4) as threads:
            copies = list(threads.map(lambda _: n.call("Workflow.Start", request), range(4)))
        self.assertEqual(len({item["reference"]["resource_id"] for item in copies}), 1)
        run = copies[0]
        waiting = self.wait(run, "WaitingApproval")
        phases = self.phases(waiting)
        self.assertEqual([phases[name]["lifecycle"] for name in ("prepare", "write", "join")],
                         ["Succeeded", "WaitingApproval", "Pending"])
        write = phases["write"]["task_receipt"]["task_id"]
        path = n.state / "volumes" / volume["resource_id"] / "data" / "proof"
        self.assertFalse(path.exists())
        self.assertEqual(self.budget(scope), (1, 1))
        persisted = n.get(run["reference"]["resource_id"])["resource"]["spec"]["steps"][0]["request"]
        self.rejected("PermissionDenied", "Task.Submit", persisted)
        n.stop(signal.SIGKILL)
        n.start()
        recovered = self.wait(run, "WaitingApproval")
        self.assertEqual(self.phases(recovered)["write"]["task_receipt"], phases["write"]["task_receipt"])
        self.assertEqual(self.budget(scope), (1, 1))
        n.approve(write)
        final = self.wait(run, "Succeeded")
        self.assertEqual(final["state"]["outcome_knowledge"], "Known")
        self.assertEqual(n.call("Workflow.Start", request), final)
        self.assertEqual(path.read_text(), "once")
        self.assertEqual(self.budget(scope), (0, 3))
        tasks = set()
        for observed in final["state"]["steps"]:
            receipt = observed["task_receipt"]
            origin = receipt["origin_ref"]
            self.assertEqual(origin["origin_type"], "WorkflowRun")
            self.assertEqual(origin["origin_id"], run["reference"]["resource_id"])
            self.assertEqual(origin["step_key"], observed["step_key"])
            self.assertEqual(origin["attempt_generation"], "1")
            self.assertEqual(observed["task_ref"], n.get(receipt["task_id"])["reference"])
            self.assertEqual(n.wait(receipt["task_id"])["lifecycle"], "Succeeded")
            tasks.add(receipt["task_id"])
        self.assertEqual(len(tasks), 3)
        self.assertEqual(len(n.call("Resource.List", {"kind": "Task"})["items"]), 4)

    def test_graph_validation_grant_scope_and_definition_idempotency(self):
        n = self.node
        raw = {"key": key(), "steps": [step("first")]}
        self.rejected("PermissionDenied", "Workflow.Define", raw)
        scope = self.grant()
        for steps in ([step("same"), step("same")], [step("dangling", after=["absent"])],
                      [step("left", after=["right"]), step("right", after=["left"])],
                      [step("dup-parent"), step("later", after=["dup-parent", "dup-parent"])],
                      [step("left", after_selected=["right"]), step("right", after=["left"])],
                      [step("first"), step("join", after=["first"], after_selected=["first"])]):
            self.rejected("InvalidArgument", "Workflow.Define", dict(raw, steps=steps))
        definition = n.call("Workflow.Define", raw)
        self.assertEqual(n.call("Workflow.Define", raw), definition)
        self.rejected("IdempotencyConflict", "Workflow.Define", dict(raw, steps=[step("other")]))
        bad = self.define([step("other-tool", "node.inventory")])
        self.rejected("PermissionDenied", "Workflow.Start", self.request(bad, scope))
        self.rejected("InvalidArgument", "Workflow.Start", self.request(definition, scope, 180))
        self.assertEqual(self.budget(scope), (0, 0))
        self.assertEqual(n.call("Resource.List", {"kind": "WorkflowRun"})["items"], [])

    def test_persisted_condition_selects_one_write_and_skips_unselected_dependents(self):
        node = self.node
        volume = node.perform("volume.create", {"name": "workflow-branches"})
        grant = self.grant(["system.status", "volume.write"], ["profile-local", volume["resource_id"]], units=5)
        authority = node.call("Principal.WhoAmI", {})["authority_id"]
        selector = {"step": "observe", "pointer": "/authority_id"}
        definition = self.define([
            step("observe"),
            step("unselected", "volume.write", dict(**target(volume), name="never", content="no"),
                 ["observe"], dict(selector, equals="different-node")),
            step("dependent", after=["unselected"]),
            step("selected", "volume.write", dict(**target(volume), name="proof", content="once"),
                 ["observe"], dict(selector, equals=authority)),
            step("verify", after_selected=["unselected", "selected"]),
        ])
        self.assertEqual(definition["definition"]["dsl_version"], 2)
        request = self.request(definition, grant)
        run = node.call("Workflow.Start", request)
        pending = self.phases(self.wait(run, "WaitingApproval"))
        self.assertEqual(pending["unselected"]["reason"], "ConditionFalse")
        self.assertFalse(pending["unselected"]["condition"]["selected"])
        self.assertEqual(pending["dependent"]["skipped_dependency"], "unselected")
        for name in ("unselected", "dependent"):
            self.assertEqual(pending[name]["lifecycle"], "Skipped")
            self.assertIsNone(pending[name]["task_receipt"])
        chosen = pending["selected"]
        write = chosen["task_receipt"]["task_id"]
        evidence = chosen["condition"]
        self.assertTrue(evidence["selected"])
        self.assertEqual(evidence["source_task_ref"], pending["observe"]["task_ref"])
        parent = node.wait(pending["observe"]["task_receipt"]["task_id"])
        self.assertEqual(evidence["source_artifact_ref"], parent["result"]["artifact_refs"][0])
        node.stop(signal.SIGKILL)
        node.start()
        self.assertEqual(self.phases(self.wait(run, "WaitingApproval"))["selected"], chosen)
        node.approve(write)
        finished = self.wait(run, "Succeeded")
        self.assertEqual(node.call("Workflow.Start", request), finished)
        phases = self.phases(finished)
        self.assertEqual(phases["selected"]["condition"], evidence)
        self.assertEqual(phases["verify"]["joined_predecessors"], [
            {"step_id": "selected", "task_ref": phases["selected"]["task_ref"]}])
        tree = node.state / "volumes" / volume["resource_id"] / "data"
        self.assertEqual({path.name: path.read_text() for path in tree.iterdir()}, {"proof": "once"})
        self.assertEqual(self.budget(grant), (0, 3))

    def test_join_of_unselected_branches_has_no_task(self):
        node = self.node
        grant = self.grant(units=4)
        false = {"step": "observe", "pointer": "/authority_id", "equals": "different-node"}
        definition = self.define([step("observe"), step("branch-a", after=["observe"], when=false),
            step("branch-b", after=["observe"], when=false), step("join", after_selected=["branch-a", "branch-b"])])
        run = node.call("Workflow.Start", self.request(definition, grant))
        finished = self.phases(self.wait(run, "Succeeded"))
        self.assertEqual(finished["join"]["reason"], "NoSelectedPredecessor")
        self.assertEqual(finished["join"]["joined_predecessors"], [])
        for name in ("branch-a", "branch-b", "join"):
            self.assertEqual(finished[name]["lifecycle"], "Skipped")
            self.assertIsNone(finished[name]["task_receipt"])
        self.assertEqual(self.budget(grant), (0, 1))

    def test_condition_rejects_invalid_types_paths_and_missing_task_read(self):
        node = self.node
        grant = self.grant(units=3)
        premise = {"step": "observe", "pointer": "/authority_id", "equals": "node"}
        for predicate in (dict(premise, step="unrelated"), dict(premise, pointer="/a~2b"),
                          dict(premise, equals={"code": "node"}), dict(premise, wildcard=True)):
            self.rejected("InvalidArgument", "Workflow.Define", {"key": key(),
                "steps": [step("observe"), step("blocked", after=["observe"], when=predicate)]})
        no_read = node.call("Grant.Issue", {"principal_id": "local-owner", "tool_targets": ["profile-local"],
            "actions": ["workflow.manage", "task.submit", "system.status"], "units": 2, "ttl_seconds": 120})
        for selector in (dict(premise, pointer="/missing"), dict(premise, equals=True)):
            definition = self.define([step("observe"), step("blocked", after=["observe"], when=selector)])
            self.rejected("PermissionDenied", "Workflow.Start", self.request(definition, no_read))
            run = node.call("Workflow.Start", self.request(definition, grant))
            failed = self.wait(run, "Failed")
            blocked = self.phases(failed)["blocked"]
            self.assertEqual((blocked["lifecycle"], blocked["reason"]), ("Failed", "PreconditionFailed"))
            self.assertIsNone(blocked["task_receipt"])
        self.assertEqual(self.budget(grant), (0, 2))

    def test_cancel_before_approval_leaves_no_write_or_successor(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "workflow-cancel"})
        scope = self.grant(["volume.write", "system.status"], ["profile-local", volume["resource_id"]], units=2)
        definition = self.define([step("write", "volume.write", dict(**target(volume), name="never", content="no")),
                                  step("after", after=["write"])])
        run = n.call("Workflow.Start", self.request(definition, scope))
        waiting = self.wait(run, "WaitingApproval")
        write = self.phases(waiting)["write"]["task_receipt"]["task_id"]
        self.assertEqual(self.budget(scope), (1, 0))
        self.rejected("Conflict", "Workflow.Cancel", target(run["reference"]))
        final = n.call("Workflow.Cancel", target(waiting["reference"]))
        self.assertEqual(final["state"]["lifecycle"], "Cancelled")
        self.assertEqual(n.wait(write)["lifecycle"], "Cancelled")
        self.assertEqual(self.phases(final)["after"]["lifecycle"], "Cancelled")
        self.assertIsNone(self.phases(final)["after"]["task_receipt"])
        self.assertFalse((n.state / "volumes" / volume["resource_id"] / "data" / "never").exists())
        self.assertEqual(self.budget(scope), (0, 0))
        self.rejected("Conflict", "Workflow.Cancel", target(final["reference"]))
        self.rejected("PreconditionFailed", "Task.Retry", dict(**target(n.get(write)["reference"]), key=key()))

    def test_cordon_expiry_and_agent_revocation_remain_durable(self):
        n = self.node
        agent = n.call("Agent.Register", {"name": "workflow-author"})
        reader, _ = n.agent(["task.read"])
        scope = self.grant(actor=agent["principal_id"])
        definition = self.define([step("pending")], agent["token"])
        self.node_scheduling("Cordoned")
        expiring = n.call("Workflow.Start", self.request(definition, scope, deadline=1.3), agent["token"])
        self.assertEqual(self.fetch(expiring, agent["token"]), expiring)
        self.rejected("PermissionDenied", "Workflow.Get", {"id": expiring["reference"]["resource_id"]}, reader["token"])
        expired = self.wait(expiring, "Failed")
        self.assertEqual((expired["state"]["reason"], expired["state"]["outcome_knowledge"]),
                         ("DeadlineExceeded", "NotStarted"))
        revoked = n.call("Workflow.Start", self.request(definition, scope), agent["token"])
        n.call("Grant.Revoke", target(scope["grant_ref"]))
        n.stop(signal.SIGKILL)
        n.start()
        self.node_scheduling("Enabled")
        final = self.wait(revoked, "Failed")
        self.assertEqual((final["state"]["reason"], final["state"]["outcome_knowledge"]),
                         ("PermissionDenied", "NotStarted"))
        self.assertIsNone(self.phases(final)["pending"]["task_receipt"])
        self.assertEqual(self.budget(scope), (0, 0))
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])

    def test_queue_pressure_preserves_the_ready_step_without_reserving_units(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "workflow-queue"})
        held = n.submit("volume.write", dict(**target(volume), name="held", content="hold"))
        self.assertEqual(n.wait(held)["lifecycle"], "WaitingApproval")
        scope = self.grant(units=2)
        definition = self.define([step("first"), step("second", after=["first"])])
        run = n.call("Workflow.Start", self.request(definition, scope))
        self.rejected("QueueFull", "Workflow.Start", self.request(definition, scope))
        n.stop(signal.SIGKILL)
        n.start()
        time.sleep(0.55)
        self.assertEqual(self.fetch(run), run)
        self.assertEqual(self.budget(scope), (0, 0))
        n.call("Task.Cancel", target(n.get(held)["reference"]))
        finished = self.wait(run, "Succeeded")
        self.assertEqual(len({s["task_receipt"]["task_id"] for s in finished["state"]["steps"]}), 2)
        self.assertEqual(self.budget(scope), (0, 2))

    def test_workflow_owner_queue_pressure_does_not_block_another_principal(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=4,
                                            principal_queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "workflow-admission-fairness"})
        held = n.submit("volume.write", {**target(volume), "name": "held", "content": "hold"})
        self.assertEqual(n.wait(held)["lifecycle"], "WaitingApproval")

        blocked_scope = self.grant(units=1)
        blocked_def = self.define([step("older")])
        blocked_request = self.request(blocked_def, blocked_scope)
        blocked = n.call("Workflow.Start", blocked_request)
        blocked_id = blocked["reference"]["resource_id"]
        pending_sql = "SELECT run_rev,step FROM workflow_admission_deferrals WHERE run=?"
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            original_spec = conn.execute("SELECT spec FROM versions WHERE id=? AND rev=1", (blocked_id,)).fetchone()
            original = json.loads(original_spec[0])["steps"][0]["request"]
            deadline = time.monotonic() + 6
            while conn.execute(pending_sql, (blocked_id,)).fetchone() != (1, "older"):
                self.assertLess(time.monotonic(), deadline, "older ready run did not hit its owner's full queue")
                time.sleep(0.05)

        independent = n.call("Agent.Register", {"name": "independent-workflow"})
        independent_scope = self.grant(units=1, actor=independent["principal_id"])
        independent_def = self.define([step("later")], independent["token"])
        later = n.call("Workflow.Start", self.request(independent_def, independent_scope), independent["token"])
        finished_later = self.wait(later, "Succeeded")
        later_receipt = self.phases(finished_later)["later"]["task_receipt"]
        self.assertEqual(self.budget(independent_scope), (0, 1))
        self.assertEqual(self.fetch(blocked), blocked)
        self.assertEqual(self.budget(blocked_scope), (0, 0))
        self.assertEqual(n.wait(held)["lifecycle"], "WaitingApproval")

        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            self.assertEqual(conn.execute(pending_sql, (blocked_id,)).fetchone(), (1, "older"))
            self.assertEqual(conn.execute("SELECT count(*) FROM current WHERE kind='Task' "
                                          "AND body->'origin_ref'->>'origin_id'=?", (blocked_id,)).fetchone(), (0,))
        subprocess.run(n.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)
        with closing(sqlite3.connect(n.state / "state.db")) as conn, conn:
            conn.execute("UPDATE workflow_admission_deferrals SET run_rev=2 WHERE run=?", (blocked_id,))
        invalid = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("RecoveryRequired", invalid.stderr)
        with closing(sqlite3.connect(n.state / "state.db")) as conn, conn:
            conn.execute("UPDATE workflow_admission_deferrals SET run_rev=1 WHERE run=?", (blocked_id,))
        n.start()
        self.assertEqual(self.fetch(blocked), blocked)
        self.assertEqual(self.phases(self.fetch(later))["later"]["task_receipt"], later_receipt)

        n.call("Task.Cancel", target(n.get(held)["reference"]))
        finished = self.wait(blocked, "Succeeded")
        receipt = self.phases(finished)["older"]["task_receipt"]
        self.assertEqual(receipt["origin_ref"], original["origin_ref"])
        self.assertEqual(receipt["idempotency_key"], original["idempotency_key"])
        self.assertEqual(receipt["request_digest"], original["request_digest"])
        self.assertEqual(n.call("Workflow.Start", blocked_request), finished)
        self.assertEqual(self.budget(blocked_scope), (0, 1))
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            self.assertIsNone(conn.execute(pending_sql, (blocked_id,)).fetchone())
            self.assertEqual(conn.execute("SELECT count(*) FROM submissions WHERE task=?",
                                          (receipt["task_id"],)).fetchone(), (1,))
