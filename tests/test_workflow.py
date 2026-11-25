"""Run literal Task DAGs through the real authority, approval and crash recovery."""
import base64
from concurrent.futures import ThreadPoolExecutor
import hmac
import json
import signal
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

    def define(self, steps, token=None):
        return self.node.call("Workflow.Define", {"key": key(), "steps": steps}, token)

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
