"""Exercise dispatch limits using real tasks, delegations and a lost effect receipt."""
import os
import signal
import sqlite3
import unittest

from support import Failure, Node, target


class TaskConcurrencyTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)

    def submit(self, actor, delegation, tool, features):
        token = actor["token"]
        request = self.n.prepare(tool, features, token=token, delegation=delegation["resource_id"])
        return self.n.call("Task.Submit", request, token)["task_id"]

    def grant(self, actor, actions, **options):
        return self.n.call("Grant.Issue", dict(
            principal_id=actor["principal_id"], actions=["task.submit", "task.read", *actions],
            ttl_seconds=180, units=4, **options))

    def receipt(self, task):
        return self.n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})

    def test_shared_budget_waits_for_unknown_effect_without_blocking_other_grants(self):
        n = self.n
        parent = n.call("Agent.Register", {"name": "budget-parent"})
        child = n.call("Agent.Register", {"name": "budget-child"})
        setup = self.grant(parent, ["volume.create"])
        provision = self.submit(parent, setup["delegation_ref"], "volume.create", {"name": "budget-volume"})
        volume = self.receipt(n.wait(provision))
        source = n.state / "volumes" / volume["resource_id"] / "data" / "record"

        for invalid in (0, 17):
            with self.assertRaises(Failure) as error:
                self.grant(parent, ["system.status"], task_concurrency=invalid)
            self.assertEqual(error.exception.code, "InvalidArgument")
        grant = self.grant(parent, ["volume.write", "system.status"], task_concurrency=1,
                           allow_delegation=True, max_delegation_depth=1,
                           tool_targets=[volume["resource_id"], "profile-local"])
        budget_id = grant["budget_ref"]["resource_id"]
        delegation = n.call("Delegation.Create", {
            "parent_id": grant["delegation_ref"]["resource_id"], "principal_id": child["principal_id"],
            "actions": ["task.submit", "task.read", "system.status"], "ttl_seconds": 120,
            "tool_targets": ["profile-local"],
        }, parent["token"])
        independent = self.grant(parent, ["system.status"], task_concurrency=1, tool_targets=["profile-local"])
        limited = lambda: n.call("Budget.Get", {"id": budget_id}, parent["token"])
        self.assertEqual(limited()["task_concurrency_remaining"], 1)
        with self.assertRaises(Failure) as hidden:
            n.call("Budget.Get", {"id": budget_id}, child["token"])
        self.assertEqual(hidden.exception.code, "PermissionDenied")

        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute(f"""CREATE TRIGGER reject_budgeted_effect BEFORE INSERT ON versions
                WHEN NEW.id='{volume["resource_id"]}' AND NEW.kind='Volume'
                BEGIN SELECT RAISE(ABORT, 'budget test loses effect receipt'); END""")
        write = self.submit(parent, grant["delegation_ref"], "volume.write", {
            **target(volume), "name": "record", "content": "one committed host effect",
        })
        lost = n.wait(write, autoapprove=True)
        self.assertEqual((lost["lifecycle"], lost["outcome_knowledge"]), ("Paused", "Unknown"), lost)
        self.assertEqual(source.read_text(), "one committed host effect")
        occupied = limited()
        self.assertEqual((occupied["task_concurrency_maximum"], occupied["task_concurrency_occupied"],
                          occupied["task_concurrency_unknown"], occupied["task_concurrency_remaining"]),
                         (1, 1, 1, 0))

        shared = self.submit(child, delegation, "system.status", {})
        separate = self.submit(parent, independent["delegation_ref"], "system.status", {})
        self.assertEqual(n.wait(separate)["lifecycle"], "Succeeded")
        queued = n.call("Task.Get", {"id": shared}, child["token"])
        self.assertEqual(queued["lifecycle"], "Queued")
        self.assertNotIn("current_run_id", queued)
        self.assertEqual((limited()["reserved"], limited()["spent"]), (1, 1))

        n.stop(signal.SIGKILL)
        with sqlite3.connect(n.state / "state.db", timeout=10) as database:
            database.execute("DROP TRIGGER reject_budgeted_effect")
        n.start()
        self.assertEqual(limited()["task_concurrency_unknown"], 1)
        self.assertEqual(n.call("Task.Get", {"id": shared}, child["token"])["lifecycle"], "Queued")
        original = (source.stat().st_ino, source.stat().st_mtime_ns)
        n.call("Operation.Reconcile", target(lost["operation_refs"][0]))
        self.assertEqual(n.wait(write)["lifecycle"], "Succeeded")
        self.assertEqual(n.wait(shared)["lifecycle"], "Succeeded")
        final = limited()
        self.assertEqual((final["task_concurrency_occupied"], final["task_concurrency_remaining"],
                          final["reserved"], final["spent"]), (0, 1, 0, 2))
        self.assertEqual((source.stat().st_ino, source.stat().st_mtime_ns), original)


if __name__ == "__main__":
    unittest.main()
