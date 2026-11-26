"""Exercise private Task preparations through the real Unix authority."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target


PRIOR_BINARIES = os.getenv("PSIOS_PRIOR_BIN_DIR")
BINARIES = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")


def in_seconds(seconds):
    return (datetime.now(timezone.utc) + timedelta(seconds=seconds)).isoformat().replace("+00:00", "Z")


def another_direct_request(existing):
    identity = key()
    request = {**existing, "origin_ref": {**existing["origin_ref"], "origin_id": identity},
               "idempotency_key": identity, "cancel_scope_id": identity}
    del request["request_digest"]
    body = json.dumps(request, sort_keys=True, separators=(",", ":")).encode()
    request["request_digest"] = "sha256:" + hashlib.sha256(b"psios/task-submit/1\n" + body).hexdigest()
    return request


class TaskPreparationTests(unittest.TestCase):
    def start_node(self, binaries=BINARIES):
        node = Node(binaries)
        self.addCleanup(node.close)
        return node

    def denied(self, n, code, method, params, token=None):
        with self.assertRaises(Failure) as caught:
            n.call(method, params, token)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def prepare(self, n, *, token=None, delegation="owner-delegation", identity=None,
                tool="system.status", data=None, deadline=None):
        params = {"tool": tool, "input": data if data is not None else {},
                  "key": identity or key(), "delegation_id": delegation}
        if deadline is not None:
            params["deadline_at"] = deadline
        return n.call("Task.Prepare", params, token)

    def lookup(self, n, request, status, token=None):
        result = n.call("Task.LookupSubmission", request, token)
        self.assertEqual(result["status"], status, result)
        self.assertEqual(result["request_digest"], request["request_digest"])
        return result

    def resolve(self, n, reference, token=None):
        return n.call("Resource.Resolve", {"reference": reference}, token)

    def list_ids(self, n, *kinds, token=None):
        return {kind: n.call("Resource.List", {"kind": kind}, token)["items"] for kind in kinds}

    def durable_input_count(self, n, owner):
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db:
            count, = db.execute("SELECT count(*) FROM versions WHERE owner=? "
                                "AND kind IN ('Input', 'TaskInputManifest')", (owner,)).fetchone()
            return count

    def restart(self, n):
        n.stop(signal.SIGKILL)
        n.start()

    def verify(self, n):
        n.stop()
        result = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertFalse(result["migration_required"], result)
        self.assertEqual((result["sqlite_integrity"], result["foreign_keys"], result["witness"]),
                         ("Verified", "Verified", "Confirmed"), result)
        return result

    def scheduling(self, n, mode):
        node = n.call("Node.Get", {})["reference"]
        n.call("Node.Configure", {"expected_resource_version": node["version"], "scheduling": mode})

    def test_live_request_quota_is_per_actor_and_creates_no_durable_inputs(self):
        n = self.start_node()
        actions = ["task.submit", "task.read", "resource.read", "event.read", "system.status"]
        actor, grant = n.agent(actions, units=1)
        peer, peer_grant = n.agent(actions, units=1)
        token = actor["token"]
        delegation = grant["delegation_ref"]["resource_id"]
        budget = {"id": grant["budget_ref"]["resource_id"]}
        initial_budget = n.call("Budget.Get", budget)
        initial = self.list_ids(n, "Input", "TaskInputManifest", "Task", "Run", "Operation", "ApprovalRequest")
        watch = n.call("Resource.List", {"kind": "Input"}, token)["watch_start_cursor"]

        first = self.prepare(n, token=token, delegation=delegation)
        reference = first["input_ref"]
        self.assertTrue(reference["resource_id"].startswith("input-prepared-"), reference)
        for slot in range(1, 128):
            with self.subTest(slot=slot):
                another = self.prepare(n, token=token, delegation=delegation)
                self.assertEqual(another["input_ref"], reference)
                self.assertNotEqual(another["request_digest"], first["request_digest"])
        overflow = {"tool": "system.status", "input": {}, "delegation_id": delegation, "key": key()}
        self.denied(n, "QuotaExceeded", "Task.Prepare", overflow, token)
        self.assertEqual(self.prepare(n, token=token, delegation=delegation,
                                      identity=first["idempotency_key"]), first)
        self.denied(n, "IdempotencyConflict", "Task.Prepare", {
            **overflow, "key": first["idempotency_key"], "deadline_at": in_seconds(15),
        }, token)
        self.assertEqual(n.call("Resource.Get", {"id": reference["resource_id"]}, token)["reference"], reference)
        self.assertEqual(self.resolve(n, reference, token)["value"], {})
        self.lookup(n, first, "AbsentWithinWindow", token)
        self.assertEqual(n.call("Resource.Watch", {"cursor": watch}, token)["events"], [])
        self.assertEqual(self.durable_input_count(n, actor["principal_id"]), 0)
        self.assertEqual(self.list_ids(n, *initial), initial)
        self.assertEqual(n.call("Budget.Get", budget), initial_budget)

        other = self.prepare(n, token=peer["token"], delegation=peer_grant["delegation_ref"]["resource_id"])
        self.assertNotEqual(other["input_ref"], reference)
        self.denied(n, "PreconditionFailed", "Resource.Resolve", {"reference": reference}, peer["token"])
        self.denied(n, "PermissionDenied", "Task.DiscardPreparation", first, peer["token"])
        self.assertEqual(n.call("Task.DiscardPreparation", first, token)["status"], "Discarded")
        self.assertEqual(n.call("Task.Prepare", overflow, token)["input_ref"], reference)
        self.lookup(n, first, "Expired", token)
        self.verify(n)

    def test_discard_and_cancel_survive_shared_input_promotion_and_task_retry(self):
        n = self.start_node()
        self.scheduling(n, "Cordoned")
        discarded, cancelled, live = (self.prepare(n) for _ in range(3))
        draft_ref = discarded["input_ref"]
        self.assertEqual(cancelled["input_ref"], draft_ref)
        self.assertEqual(live["input_ref"], draft_ref)

        schedule_grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["schedule.manage", "task.submit", "task.read",
                                                        "resource.read", "event.read", "system.status"],
            "tool_targets": ["profile-local"], "ttl_seconds": 90, "units": 1,
        })
        schedule = {"key": key(), "tool": "system.status", "input": {},
                    "delegation_id": schedule_grant["delegation_ref"]["resource_id"],
                    "event": {"source_ref": draft_ref, "condition": {"kind": "SpecChanged"},
                              "expires_at": in_seconds(60), "max_triggers": 1}}
        self.lookup(n, live, "AbsentWithinWindow")
        self.denied(n, "PreconditionFailed", "Schedule.Create", schedule)
        self.assertEqual(self.list_ids(n, "Schedule")["Schedule"], [])
        self.assertEqual(self.resolve(n, draft_ref)["reference"], draft_ref)

        request_file = n.base / "discard-preparation.json"
        request_file.write_text(json.dumps(discarded))
        request_file.chmod(0o600)
        drop = n.cli(n.witness / "owner.token", "task-discard-preparation", request_file)
        self.assertEqual(drop["status"], "Discarded", drop)
        cancel = {name: cancelled[name] for name in ("schema_version", "origin_ref", "idempotency_key",
                                                     "request_digest", "cancel_scope_id")}
        cancel.update(reason="sent before accepting the task", cancel_request_id=key())
        blocked = n.call("Task.CancelOrigin", cancel)
        self.assertEqual(blocked["status"], "Blocked", blocked)
        self.restart(n)
        self.assertEqual(n.call("Task.DiscardPreparation", discarded)["status"], "AlreadyDiscarded")
        self.assertEqual(n.call("Task.CancelOrigin", cancel), blocked)
        self.assertEqual(self.prepare(n, identity=live["idempotency_key"]), live)
        self.assertEqual(self.resolve(n, draft_ref)["value"], {})

        accepted = n.call("Task.Submit", live)
        self.assertFalse(accepted["deduplicated"], accepted)
        task_id = accepted["task_id"]
        task = n.get(task_id)
        input_ref = task["resource"]["spec"]["input_ref"]
        self.assertIn(input_ref, self.list_ids(n, "Input")["Input"])
        self.assertEqual(self.resolve(n, input_ref)["value"], {})
        self.assertEqual(self.durable_input_count(n, "local-owner"), 1)
        self.assertEqual(self.lookup(n, live, "Found")["accepted"]["task_id"], task_id)
        self.assertEqual(n.call("Task.DiscardPreparation", live)["status"], "AlreadyAccepted")
        self.lookup(n, discarded, "Expired")
        self.lookup(n, cancelled, "Expired")
        self.denied(n, "Cancelled", "Task.Submit", cancelled)
        self.denied(n, "Cancelled", "Task.Submit", discarded)
        for closed in (discarded, cancelled):
            self.denied(n, "Cancelled", "Task.Prepare", {
                "tool": "system.status", "input": {}, "delegation_id": "owner-delegation",
                "key": closed["idempotency_key"],
            })
        unprepared = another_direct_request(live)
        self.lookup(n, unprepared, "Expired")
        self.denied(n, "DeadlineExceeded", "Task.Submit", unprepared)

        n.call("Task.Cancel", target(task["reference"]))
        stopped = n.wait(task_id)
        self.assertEqual((stopped["lifecycle"], stopped["outcome_knowledge"]), ("Cancelled", "NotStarted"), stopped)
        n.call("Task.Archive", target(n.get(task_id)["reference"]))
        self.restart(n)
        replay = n.call("Task.Submit", live)
        self.assertEqual(replay, {**accepted, "deduplicated": True})
        retried = n.call("Task.Retry", {**target(n.get(task_id)["reference"]), "key": key()})
        self.assertNotEqual(retried["task_id"], task_id)
        self.assertEqual(n.get(retried["task_id"])["resource"]["spec"]["input_ref"], input_ref)
        self.scheduling(n, "Enabled")
        successor = n.wait(retried["task_id"])
        self.assertEqual((successor["lifecycle"], successor["retry_of_task_id"]), ("Succeeded", task_id))
        self.assertEqual(self.resolve(n, input_ref)["reference"], input_ref)
        schedule["event"]["source_ref"] = input_ref
        rule = n.call("Schedule.Create", schedule)
        self.assertEqual((rule["state"]["lifecycle"], rule["state"]["source_ref"]), ("Pending", input_ref))
        self.verify(n)

    def test_elapsed_preparation_is_fenced_across_restart_and_shared_content_prepare(self):
        n = self.start_node()
        short = self.prepare(n, deadline=in_seconds(4))
        original_ref = short["input_ref"]
        self.lookup(n, short, "AbsentWithinWindow")
        self.restart(n)
        deadline = time.monotonic() + 12
        while True:
            lookup = n.call("Task.LookupSubmission", short)
            if lookup["status"] == "Expired":
                break
            self.assertEqual(lookup["status"], "AbsentWithinWindow", lookup)
            self.assertLess(time.monotonic(), deadline, lookup)
            time.sleep(0.05)
        self.denied(n, "PreconditionFailed", "Resource.Resolve", {"reference": original_ref})
        self.denied(n, "DeadlineExceeded", "Task.DiscardPreparation", short)
        unrelated = self.prepare(n)
        self.assertEqual(unrelated["input_ref"], original_ref)
        self.lookup(n, unrelated, "AbsentWithinWindow")
        self.lookup(n, short, "Expired")
        accepted = n.call("Task.Submit", unrelated)
        self.assertEqual(n.wait(accepted["task_id"])["lifecycle"], "Succeeded")
        self.lookup(n, short, "Expired")
        self.denied(n, "DeadlineExceeded", "Task.Submit", short)
        self.assertEqual(len(self.list_ids(n, "Task")["Task"]), 1)
        self.restart(n)
        until = time.monotonic() + 5
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db:
            while db.execute("SELECT 1 FROM task_preparations WHERE key=?",
                             (short["idempotency_key"],)).fetchone():
                self.assertLess(time.monotonic(), until, "expired preparation was not collected")
                time.sleep(0.05)
            self.assertEqual(db.execute("SELECT count(*) FROM task_prepared_inputs").fetchone(), (0,))
        self.assertEqual(self.resolve(n, original_ref)["value"], {})
        self.lookup(n, short, "Expired")
        self.verify(n)

    def test_failed_promotion_preserves_exact_preparation_for_atomic_retry(self):
        n = self.start_node()
        request = self.prepare(n)
        original_input = self.resolve(n, request["input_ref"])
        initial = self.list_ids(n, "Input", "OperationPlan", "Operation", "Task", "Run")
        budget = {"id": request["budget_ref"]["resource_id"]}
        initial_budget = n.call("Budget.Get", budget)
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db, db:
            db.execute("""CREATE TRIGGER refuse_prepared_task BEFORE INSERT ON versions
                WHEN NEW.kind='Task'
                BEGIN SELECT RAISE(ABORT, 'injected failure before accepting preparation'); END""")
        try:
            self.denied(n, "StorageUnavailable", "Task.Submit", request)
            self.assertEqual(self.list_ids(n, *initial), initial)
            self.assertEqual(self.durable_input_count(n, "local-owner"), 0)
            self.assertEqual(n.call("Budget.Get", budget), initial_budget)
            self.assertEqual(self.resolve(n, request["input_ref"]), original_input)
            self.lookup(n, request, "AbsentWithinWindow")
            self.restart(n)
            self.lookup(n, request, "AbsentWithinWindow")
            self.assertEqual(self.prepare(n, identity=request["idempotency_key"]), request)
        finally:
            with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as db, db:
                db.execute("DROP TRIGGER refuse_prepared_task")

        with ThreadPoolExecutor(max_workers=2) as callers:
            receipts = list(callers.map(lambda _: n.call("Task.Submit", request), range(2)))
        self.assertEqual(sorted(receipt["deduplicated"] for receipt in receipts), [False, True])
        first, second = receipts
        self.assertEqual(first["task_id"], second["task_id"])
        self.assertEqual(self.durable_input_count(n, "local-owner"), 1)
        self.assertEqual(n.wait(first["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(len(self.list_ids(n, "Task")["Task"]), 1)
        self.assertEqual(self.lookup(n, request, "Found")["accepted"]["task_id"], first["task_id"])
        self.verify(n)

    @unittest.skipUnless(PRIOR_BINARIES, "set PSIOS_PRIOR_BIN_DIR to actual format 95 binaries")
    def test_format95_durable_input_history_survives_upgrade(self):
        n = self.start_node(PRIOR_BINARIES)
        accepted_request = self.prepare(n)
        accepted = n.call("Task.Submit", accepted_request)
        completed = n.wait(accepted["task_id"])
        self.assertEqual(completed["lifecycle"], "Succeeded", completed)
        old_unsubmitted = self.prepare(n, tool="node.processes", data={"limit": 1})
        accepted_input = self.resolve(n, accepted_request["input_ref"])
        unsubmitted_input = self.resolve(n, old_unsubmitted["input_ref"])
        schedule_grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["schedule.manage", "task.submit", "task.read",
                                                        "resource.read", "event.read", "system.status"],
            "tool_targets": ["profile-local"], "ttl_seconds": 120, "units": 1,
        })
        legacy_rule = n.call("Schedule.Create", {
            "key": key(), "tool": "system.status", "input": {},
            "delegation_id": schedule_grant["delegation_ref"]["resource_id"],
            "event": {"source_ref": old_unsubmitted["input_ref"], "condition": {"kind": "SpecChanged"},
                      "expires_at": in_seconds(90), "max_triggers": 1},
        })
        n.stop(signal.SIGKILL)

        n.daemon[0] = str(Path(BINARIES, "psiosd").absolute())
        before = json.loads(subprocess.check_output([*n.daemon, "verify"], timeout=20))
        self.assertEqual(before["database_format"], 95, before)
        self.assertTrue(before["migration_required"], before)
        n.start()
        self.assertEqual(n.wait(accepted["task_id"]), completed)
        self.assertEqual(self.resolve(n, accepted_request["input_ref"]), accepted_input)
        self.assertEqual(self.resolve(n, old_unsubmitted["input_ref"]), unsubmitted_input)
        self.assertEqual(n.call("Task.Submit", accepted_request), {**accepted, "deduplicated": True})
        self.assertEqual(self.prepare(n, identity=accepted_request["idempotency_key"]), accepted_request)
        self.assertEqual(n.call("Schedule.Get", {"id": legacy_rule["reference"]["resource_id"]}), legacy_rule)
        self.lookup(n, old_unsubmitted, "AbsentWithinWindow")
        submitted = n.call("Task.Submit", old_unsubmitted)
        self.assertEqual(n.wait(submitted["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(n.get(submitted["task_id"])["resource"]["spec"]["input_ref"], old_unsubmitted["input_ref"])

        after = self.verify(n)
        self.assertEqual(after["database_format"], 96, after)
        self.assertEqual(after["node_id"], before["node_id"])


if __name__ == "__main__":
    unittest.main()
