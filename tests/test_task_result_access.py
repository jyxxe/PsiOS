"""Real-local grants authorize only a Task's own result and its pinned dependencies."""
import base64
import hashlib
import json
import os
import signal
import unittest

from support import Failure, Node, key, target


class TaskResultAccessTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.producer, self.producer_grant = self.n.agent([
            "task.submit", "task.read", "volume.create", "volume.read",
        ])
        created = self.submit(self.producer, self.producer_grant, "volume.create", {"name": "shared-results"})
        self.volume = self.result(created)
        self.content = {"name": "input", "content": "prepared by the producing agent"}
        path = self.n.state / "volumes" / self.volume["resource_id"] / "data" / self.content["name"]
        path.write_text(self.content["content"])
        data = json.dumps(self.content, sort_keys=True, separators=(",", ":")).encode()
        self.digest = "sha256:" + hashlib.sha256(data).hexdigest()

    def submit(self, actor, grant, tool, data=None, requires=()):
        request = self.n.call("Task.Prepare", {
            "key": key(), "tool": tool, "input": data or {},
            "delegation_id": grant["delegation_ref"]["resource_id"],
            "requires": list(requires),
        }, actor["token"])
        return self.n.call("Task.Submit", request, actor["token"])["task_id"]

    def source(self):
        return self.submit(self.producer, self.producer_grant, "volume.read",
                           {**target(self.volume), "name": self.content["name"]})

    def result(self, task):
        settled = self.n.wait(task)
        self.assertEqual(settled["lifecycle"], "Succeeded", settled)
        return self.n.call("Artifact.Read", {"reference": settled["result"]["artifact_refs"][0]})

    def share(self, actor, tasks, **options):
        return self.n.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": ["task.results.read"],
            "units": 1, "ttl_seconds": 120, "tool_targets": tasks, **options,
        })

    def scheduling(self, mode):
        ref = self.n.call("Node.Get", {})["reference"]
        self.n.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})

    def check_failure(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as caught:
            self.n.call(method, params, actor["token"] if actor is not None else None)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def test_result_and_actual_artifact_bytes_are_scoped_and_audited(self):
        n = self.n
        first, unrelated = self.source(), self.source()
        first_task, unrelated_task = n.wait(first), n.wait(unrelated)
        reference = first_task["result"]["artifact_refs"][0]
        other_ref = unrelated_task["result"]["artifact_refs"][0]
        reader = n.call("Agent.Register", {"name": "result-reader"})
        base = {"principal_id": reader["principal_id"], "actions": ["task.results.read"],
                "units": 1, "ttl_seconds": 120}
        self.check_failure("InvalidArgument", "Grant.Issue", base)
        self.check_failure("InvalidArgument", "Grant.Issue", {**base, "tool_targets": [self.volume["resource_id"]]})
        self.check_failure("PermissionDenied", "Task.Results", {"id": first}, reader)
        share = self.share(reader, [first])

        token = n.base / "reader.token"
        token.write_text(reader["token"])
        token.chmod(0o600)
        view = n.cli(token, "task-results", first)
        self.assertEqual(view["task_revision"], first_task["task_revision"])
        self.assertEqual(view["result"]["artifact_refs"], [reference])
        self.assertEqual(set(view), {"schema_version", "task_id", "task_revision", "lifecycle",
                                     "outcome_knowledge", "result", "conditions"})
        self.assertEqual(n.call("Artifact.Get", {"reference": reference}, reader["token"])["reference"], reference)
        self.assertEqual(n.call("Artifact.Read", {"reference": reference}, reader["token"]), self.content)
        page = n.call("Artifact.ReadChunk", {"reference": reference, "offset": 1, "length": 7}, reader["token"])
        self.assertEqual(base64.b64decode(page["data_base64"]), b'"conten')
        reference_file = n.base / "shared-artifact.json"
        reference_file.write_text(json.dumps(reference))
        self.assertEqual(n.cli(token, "artifact", reference_file), self.content)

        self.check_failure("PermissionDenied", "Task.Results", {"id": unrelated}, reader)
        self.check_failure("PermissionDenied", "Task.Get", {"id": first}, reader)
        self.check_failure("PermissionDenied", "Task.History", {"id": first}, reader)
        self.check_failure("PermissionDenied", "Resource.Resolve", {"reference": reference}, reader)
        for foreign in (other_ref, {**reference, "digest": "sha256:" + "0" * 64}):
            self.check_failure("PermissionDenied", "Artifact.Get", {"reference": foreign}, reader)
        n.call("Grant.Issue", {**base, "actions": ["task.read", "resource.read", "event.read"]})
        self.check_failure("PermissionDenied", "Task.Get", {"id": first}, reader)
        self.check_failure("PermissionDenied", "Resource.Resolve", {"reference": reference}, reader)
        audit = n.call("Audit.List", {"limit": 128})
        allowed = {(row["method"], row["resource"]) for row in audit
                   if row["actor"] == reader["principal_id"] and row["decision"] == "Allowed"}
        self.assertIn(("Task.Results", first), allowed)
        self.assertIn(("Artifact.ReadChunk", reference["resource_id"]), allowed)

        n.call("Task.EvictResults", {**target(n.get(first)["reference"]), "reason": "private operator note"})
        metadata = n.call("Artifact.Get", {"reference": reference}, reader["token"])
        self.assertEqual(metadata["availability"], "Evicted")
        self.assertIsNone(metadata["retention"])
        self.check_failure("ResultEvicted", "Artifact.Read", {"reference": reference}, reader)
        n.call("Grant.Revoke", target(share["grant_ref"]))
        self.check_failure("PermissionDenied", "Task.Results", {"id": first}, reader)
        self.check_failure("PermissionDenied", "Artifact.ReadChunk",
                           {"reference": reference, "offset": 0, "length": 1}, reader)

    def test_revoke_ancestor_fences_a_persisted_wait_and_replacement_needs_new_admission(self):
        n = self.n
        reader, execution = n.agent(["task.submit", "task.read", "system.status"])
        relay = n.call("Agent.Register", {"name": "scoped-relay"})
        self.scheduling("Cordoned")
        upstream, other = self.source(), self.source()
        parent = self.share(relay, [upstream, other], allow_delegation=True, max_delegation_depth=2)
        n.call("Delegation.Create", {
            "parent_id": parent["delegation_ref"]["resource_id"], "principal_id": reader["principal_id"],
            "actions": ["task.results.read"], "ttl_seconds": 90, "tool_targets": [upstream],
        }, relay["token"])
        self.assertEqual(n.call("Task.Results", {"id": upstream}, reader["token"])["lifecycle"], "Queued")
        self.check_failure("PermissionDenied", "Task.Results", {"id": other}, reader)
        required = [{"task_id": upstream, "digest": self.digest}]
        dependent = self.submit(reader, execution, "system.status", requires=required)
        before = n.call("Task.Get", {"id": dependent})
        self.assertEqual(before["lifecycle"], "WaitingResource")
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Task.Get", {"id": dependent}), before)

        replacement = self.share(reader, [upstream])
        self.assertEqual(n.call("Grant.Revoke", target(parent["grant_ref"]))["status"], "Complete")
        fenced = n.wait(dependent)
        self.assertEqual((fenced["lifecycle"], fenced["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertEqual(fenced["conditions"][0]["type"], "PermissionDenied")
        self.assertNotIn("current_run_id", fenced)
        self.assertEqual(n.call("Task.Get", {"id": upstream})["lifecycle"], "Queued")
        budget = n.call("Budget.Get", {"id": execution["budget_ref"]["resource_id"]})
        self.assertEqual((budget["reserved"], budget["spent"]), (0, 0))

        self.scheduling("Enabled")
        self.assertEqual(self.result(upstream), self.content)
        ref = n.call("Task.Results", {"id": upstream}, reader["token"])["result"]["artifact_refs"][0]
        self.assertEqual(ref["digest"], self.digest)
        self.scheduling("Cordoned")
        second = self.submit(reader, execution, "system.status", requires=required)
        self.check_failure("Conflict", "Task.EvictResults",
                           {**target(n.get(upstream)["reference"]), "reason": "active external consumer"})
        self.scheduling("Enabled")
        completed = n.wait(second)
        self.assertEqual(completed["lifecycle"], "Succeeded")
        run = n.call("Run.Get", {"id": completed["current_run_id"]})
        snapshot = n.get(run["execution_snapshot_ref"]["resource_id"])["value"]
        self.assertEqual(snapshot["required_artifact_refs"], [ref])
        n.call("Grant.Revoke", target(replacement["grant_ref"]))
        self.check_failure("PermissionDenied", "Artifact.Read", {"reference": ref}, reader)

    def test_expired_result_lease_releases_queued_dependency_while_cordoned(self):
        n = self.n
        reader, execution = n.agent(["task.submit", "task.read", "system.status"])
        upstream = self.source()
        self.assertEqual(self.result(upstream), self.content)
        self.scheduling("Cordoned")
        self.share(reader, [upstream], ttl_seconds=3)
        consumer = self.submit(reader, execution, "system.status",
                               requires=[{"task_id": upstream, "digest": self.digest}])
        self.assertEqual(n.call("Task.Get", {"id": consumer})["lifecycle"], "Queued")
        expired = n.wait(consumer)
        self.assertEqual(expired["outcome_knowledge"], "NotStarted")
        self.assertEqual(expired["conditions"][0]["type"], "DeadlineExceeded")
        self.assertNotIn("current_run_id", expired)
        self.check_failure("PermissionDenied", "Task.Results", {"id": upstream}, reader)
        n.call("Task.EvictResults", {**target(n.get(upstream)["reference"]), "reason": "lease released"})

    def test_revoking_result_delegation_invalidates_already_approved_write(self):
        n = self.n
        reader, execution = n.agent(["task.submit", "task.read", "volume.create", "volume.write"])
        volume = self.result(self.submit(reader, execution, "volume.create", {"name": "shared-consumer"}))
        upstream = self.source()
        self.assertEqual(self.result(upstream), self.content)
        self.scheduling("Cordoned")
        share = self.share(reader, [upstream])
        downstream = self.submit(reader, execution, "volume.write",
                                 {**target(volume), "name": "fenced", "content": "approved write"},
                                 [{"task_id": upstream, "digest": self.digest}])
        approval = n.approve(downstream)
        self.assertEqual(n.call("Task.Get", {"id": downstream})["lifecycle"], "Queued")
        use_id = "use-" + approval["decision_id"]
        self.assertEqual(n.get(use_id)["value"]["validity_status"], "Valid")
        n.call("Delegation.Revoke", target(share["delegation_ref"]))
        stopped = n.wait(downstream)
        self.assertEqual((stopped["lifecycle"], stopped["outcome_knowledge"]), ("Failed", "NotStarted"))
        self.assertEqual(stopped["conditions"][0]["type"], "PermissionDenied")
        self.assertEqual(n.get(use_id)["value"]["validity_status"], "Revoked")
        self.scheduling("Enabled")
        self.assertFalse((n.state / "volumes" / volume["resource_id"] / "data" / "fenced").exists())
