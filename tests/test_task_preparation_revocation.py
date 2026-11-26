"""Retire private Task preparations when their exact authority is withdrawn."""

from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target


BINARIES = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")
PRIOR_BINARIES = os.getenv("PSIOS_PREPARATION_PRIOR_BIN_DIR")
ACTIONS = ["task.submit", "task.read", "resource.read", "system.status", "node.processes"]
REQUEST_LIMIT = 128
BYTE_LIMIT = 8 * 1024 * 1024


def body_bytes(value):
    return len(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


class TaskPreparationRevocationTests(unittest.TestCase):
    def start_node(self, binaries=BINARIES):
        node = Node(binaries)
        self.addCleanup(node.close)
        return node

    def grant(self, node, actor, **scope):
        return node.call("Grant.Issue", {
            "principal_id": actor["principal_id"], "actions": ACTIONS, "ttl_seconds": 600,
            "units": 4, **scope,
        })

    def prepare(self, node, actor, delegation, *, query=None, identity=None):
        tool, body = ("system.status", {}) if query is None else ("node.processes", query)
        return node.prepare(tool, body, identity, actor["token"], delegation["resource_id"])

    def denied(self, node, expected, method, params, actor):
        with self.assertRaises(Failure) as caught:
            node.call(method, params, actor["token"])
        self.assertEqual(caught.exception.code, expected, caught.exception)

    def usage(self, node, actor):
        snapshot = node.call("Task.Admission", {"include_preparations": True}, actor["token"])
        self.assertEqual(snapshot["principal_id"], actor["principal_id"], snapshot)
        self.assertEqual(snapshot["preparations"]["requests"]["limit"], REQUEST_LIMIT, snapshot)
        self.assertEqual(snapshot["preparations"]["bytes"]["limit"], BYTE_LIMIT, snapshot)
        self.assertEqual(snapshot["ordinary"]["used"], 0, snapshot)
        return snapshot["preparations"]["requests"]["used"], snapshot["preparations"]["bytes"]["used"]

    def resolve(self, node, actor, request):
        return node.call("Resource.Resolve", {"reference": request["input_ref"]}, actor["token"])

    def lookup(self, node, actor, request, status):
        result = node.call("Task.LookupSubmission", request, actor["token"])
        self.assertEqual(result["status"], status, result)
        self.assertEqual(result["request_digest"], request["request_digest"], result)
        return result

    def blocked(self, node, actor, request):
        self.denied(node, "PermissionDenied", "Task.Submit", request, actor)
        self.lookup(node, actor, request, "Expired")

    def wait_until(self, observe, expected, message):
        deadline = time.monotonic() + 15
        while True:
            observed = observe()
            if observed == expected:
                return
            self.assertLess(time.monotonic(), deadline, f"{message}: {observed!r}, expected {expected!r}")
            time.sleep(0.05)

    def storage(self, node, actor):
        # A revoked subject can no longer call Task.Admission. Observe its own
        # test database without issuing any request that could reclaim a row.
        name = (node.state / "state.db").as_uri() + "?mode=ro"
        subject = actor["principal_id"]
        with closing(sqlite3.connect(name, uri=True, timeout=10)) as db:
            return db.execute("""SELECT
                (SELECT count(*) FROM task_preparations WHERE owner=:subject),
                (SELECT count(*) FROM task_prepared_inputs WHERE owner=:subject),
                (SELECT coalesce(sum(bytes),0) FROM task_prepared_inputs WHERE owner=:subject)""",
                              {"subject": subject}).fetchone()

    def restart(self, node):
        node.stop(signal.SIGKILL)
        node.start()

    def verify(self, node):
        node.stop()
        report = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        self.assertFalse(report["migration_required"], report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"], report["witness"]),
                         ("Verified", "Verified", "Confirmed"), report)

    def test_grant_revoke_sweeps_all_pages_but_keeps_other_exact_scopes(self):
        n = self.start_node()
        worker = n.call("Agent.Register", {"name": "preparation-worker"})
        observer, _ = n.agent(ACTIONS)
        withdrawn = self.grant(n, worker)
        retained = self.grant(n, worker)
        old_scope, live_scope = (grant["delegation_ref"] for grant in (withdrawn, retained))
        old_task_request = self.prepare(n, worker, old_scope, query={"limit": 3})
        live_task_request = self.prepare(n, worker, live_scope, query={"limit": 4})
        old_receipt = n.call("Task.Submit", old_task_request, worker["token"])
        live_receipt = n.call("Task.Submit", live_task_request, worker["token"])
        old_task = n.wait(old_receipt["task_id"])
        live_task = n.wait(live_receipt["task_id"])
        self.assertEqual((old_task["lifecycle"], live_task["lifecycle"]), ("Succeeded", "Succeeded"))
        old_durable_input = self.resolve(n, worker, old_task_request)
        budgets = [{"id": grant["budget_ref"]["resource_id"]} for grant in (withdrawn, retained)]
        before = [n.call("Budget.Get", budget) for budget in budgets]

        survivors = [self.prepare(n, worker, live_scope) for _ in range(3)]
        sample = self.prepare(n, worker, old_scope)
        private_query = {"limit": 1}
        private_only = self.prepare(n, worker, old_scope, query=private_query)
        for _ in range(123):
            self.assertEqual(self.prepare(n, worker, old_scope)["input_ref"], sample["input_ref"])
        self.assertTrue(all(request["input_ref"] == sample["input_ref"] for request in survivors))
        shared_bytes = body_bytes({})
        self.assertEqual(self.usage(n, worker), (REQUEST_LIMIT, shared_bytes + body_bytes(private_query)))
        self.assertEqual(self.usage(n, observer), (0, 0))
        default_view = n.call("Task.Admission", {}, worker["token"])
        self.assertNotIn("preparations", default_view)
        token_file = n.base / "preparation-worker.token"
        token_file.write_text(worker["token"])
        token_file.chmod(0o600)
        cli_view = n.cli(token_file, "task-admission", "--include-preparations")
        self.assertEqual(cli_view, n.call("Task.Admission", {"include_preparations": True}, worker["token"]))
        self.denied(n, "InvalidArgument", "Task.Admission", {"principal_id": worker["principal_id"]}, observer)
        self.denied(n, "InvalidArgument", "Task.Admission", {"principal_id": worker["principal_id"]},
                    {"token": n.owner})
        self.denied(n, "PreconditionFailed", "Resource.Resolve", {"reference": sample["input_ref"]}, observer)
        with self.assertRaises(Failure) as overflow:
            retry_key = key()
            self.prepare(n, worker, live_scope, identity=retry_key)
        self.assertEqual(overflow.exception.code, "QuotaExceeded", overflow.exception)
        self.assertEqual(self.resolve(n, worker, private_only)["value"], private_query)
        self.lookup(n, worker, sample, "AbsentWithinWindow")

        revoked = n.call("Grant.Revoke", target(withdrawn["grant_ref"]))
        self.assertEqual(revoked["status"], "Complete", revoked)
        self.blocked(n, worker, sample)
        self.denied(n, "PermissionDenied", "Task.Submit", old_task_request, worker)
        self.denied(n, "PreconditionFailed", "Resource.Resolve", {"reference": private_only["input_ref"]}, worker)
        self.assertEqual(self.resolve(n, worker, survivors[0])["value"], {})

        # There are 125 denied preparations. Reading usage must eventually
        # show that the real background collector passed the first 32 rows.
        self.wait_until(lambda: self.usage(n, worker), (3, shared_bytes), "revoked quota was not reclaimed")
        self.assertEqual(self.storage(n, worker), (3, 1, shared_bytes))
        self.assertEqual([n.call("Budget.Get", budget) for budget in budgets], before)
        self.assertEqual(n.call("Task.Get", {"id": old_receipt["task_id"]}, worker["token"]), old_task)
        self.assertEqual(self.resolve(n, worker, old_task_request), old_durable_input)

        self.restart(n)
        self.assertEqual(self.usage(n, worker), (3, shared_bytes))
        self.assertEqual(n.call("Task.Submit", live_task_request, worker["token"]),
                         {**live_receipt, "deduplicated": True})
        for request in survivors:
            self.lookup(n, worker, request, "AbsentWithinWindow")
            self.assertEqual(self.prepare(n, worker, live_scope, identity=request["idempotency_key"]), request)
        replacement = self.prepare(n, worker, live_scope, identity=retry_key)
        self.assertEqual(replacement["input_ref"], sample["input_ref"])
        self.assertEqual(self.usage(n, worker), (4, shared_bytes))
        executed = n.call("Task.Submit", replacement, worker["token"])
        self.assertEqual(n.wait(executed["task_id"])["lifecycle"], "Succeeded")
        self.blocked(n, worker, sample)
        self.assertEqual(self.usage(n, worker), (3, shared_bytes))
        self.verify(n)

    def test_revoke_ancestor_then_principal_reclaims_exact_rows(self):
        n = self.start_node()
        parent = n.call("Agent.Register", {"name": "preparation-issuer"})
        middle = n.call("Agent.Register", {"name": "preparation-delegate"})
        worker = n.call("Agent.Register", {"name": "preparation-descendant"})
        peer, peer_grant = n.agent(ACTIONS)
        root = self.grant(n, parent, allow_delegation=True, max_delegation_depth=2)
        ancestor = n.call("Delegation.Create", {
            "parent_id": root["delegation_ref"]["resource_id"], "principal_id": middle["principal_id"],
            "actions": ACTIONS, "ttl_seconds": 480, "allow_delegation": True,
        }, parent["token"])
        leaf = n.call("Delegation.Create", {
            "parent_id": ancestor["resource_id"], "principal_id": worker["principal_id"],
            "actions": ACTIONS, "ttl_seconds": 420,
        }, middle["token"])
        direct = self.grant(n, worker)
        blocked = self.prepare(n, worker, leaf)
        leaf_only = self.prepare(n, worker, leaf, query={"limit": 5})
        shared = self.prepare(n, worker, direct["delegation_ref"])
        direct_only = self.prepare(n, worker, direct["delegation_ref"], query={"limit": 6})
        peer_request = self.prepare(n, peer, peer_grant["delegation_ref"])
        self.assertEqual(blocked["input_ref"], shared["input_ref"])
        self.assertNotEqual(peer_request["input_ref"], shared["input_ref"])
        remaining_bytes = body_bytes({}) + body_bytes({"limit": 6})
        self.assertEqual(self.usage(n, worker), (4, remaining_bytes + body_bytes({"limit": 5})))

        n.call("Delegation.Revoke", target(ancestor), parent["token"])
        self.assertEqual(n.get(root["grant_ref"]["resource_id"])["value"]["lifecycle"], "Active")
        current_leaf = n.get(leaf["resource_id"])
        self.assertEqual(current_leaf["reference"], leaf)
        self.assertEqual(current_leaf["value"]["lifecycle"], "Active")
        self.blocked(n, worker, blocked)
        self.denied(n, "PreconditionFailed", "Resource.Resolve", {"reference": leaf_only["input_ref"]}, worker)
        self.wait_until(lambda: self.usage(n, worker), (2, remaining_bytes), "descendant scope remained charged")
        self.assertEqual(self.storage(n, worker), (2, 2, remaining_bytes))
        self.restart(n)
        self.assertEqual(self.usage(n, worker), (2, remaining_bytes))
        self.lookup(n, worker, shared, "AbsentWithinWindow")
        self.assertEqual(self.resolve(n, worker, direct_only)["value"], {"limit": 6})

        reference = n.get(worker["principal_id"])["reference"]
        n.call("Principal.Revoke", target(reference))
        self.denied(n, "AuthenticationRequired", "Task.Admission", {}, worker)
        self.assertEqual(n.get(direct["grant_ref"]["resource_id"])["value"]["lifecycle"], "Active")
        self.wait_until(lambda: self.storage(n, worker), (0, 0, 0), "revoked principal retained private content")
        self.restart(n)
        self.assertEqual(self.storage(n, worker), (0, 0, 0))
        self.assertEqual(self.usage(n, peer), (1, body_bytes({})))
        self.assertEqual(self.resolve(n, peer, peer_request)["value"], {})
        self.lookup(n, peer, peer_request, "AbsentWithinWindow")
        self.verify(n)

    @unittest.skipUnless(PRIOR_BINARIES, "set PSIOS_PREPARATION_PRIOR_BIN_DIR to pre-sweep format 96 binaries")
    def test_startup_sweeps_already_revoked_authority_without_new_revocation(self):
        n = self.start_node(PRIOR_BINARIES)
        actor, withdrawn = n.agent(ACTIONS)
        keep = self.grant(n, actor)
        stale = [self.prepare(n, actor, withdrawn["delegation_ref"]) for _ in range(35)]
        survivor = self.prepare(n, actor, keep["delegation_ref"])
        self.assertEqual(stale[0]["input_ref"], survivor["input_ref"])
        n.call("Grant.Revoke", target(withdrawn["grant_ref"]))
        self.blocked(n, actor, stale[-1])
        # The previously shipped authority has no revocation collector.
        self.assertEqual(self.storage(n, actor), (36, 1, body_bytes({})))
        n.stop(signal.SIGKILL)
        self.assertEqual(self.storage(n, actor), (36, 1, body_bytes({})))

        n.daemon[0] = str(Path(BINARIES, "psiosd").absolute())
        n.start()
        self.wait_until(lambda: self.storage(n, actor), (1, 1, body_bytes({})), "startup did not reclaim denied drafts")
        self.assertEqual(self.usage(n, actor), (1, body_bytes({})))
        self.blocked(n, actor, stale[-1])
        self.lookup(n, actor, survivor, "AbsentWithinWindow")
        self.assertEqual(self.resolve(n, actor, survivor)["value"], {})
        self.verify(n)


if __name__ == "__main__":
    unittest.main()
