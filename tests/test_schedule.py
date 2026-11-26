"""Schedule real Tasks through deadlines, restart, approval and competing admission."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from datetime import datetime, timedelta, timezone
import json
import os
import signal
import sqlite3
import subprocess
import time
import unittest

from support import Failure, Node, key, target


def timestamp(seconds):
    return (datetime.now(timezone.utc) + timedelta(seconds=seconds)).isoformat(timespec="milliseconds")


def after(instant, grace=0.1):
    delay = (datetime.fromisoformat(instant) - datetime.now(timezone.utc)).total_seconds() + grace
    if delay > 0:
        time.sleep(delay)


class ScheduleTests(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)

    def grant(self, tool="system.status", resource="profile-local", principal="local-owner", units=1):
        return self.node.call("Grant.Issue", dict(principal_id=principal, tool_targets=[resource],
                              actions=["schedule.manage", "task.submit", "task.read", tool],
                              units=units, ttl_seconds=60))

    def request(self, grant, tool="system.status", input=None, run=1.5, deadline=15):
        return dict(key=key(), tool=tool, input={} if input is None else input,
                    delegation_id=grant["delegation_ref"]["resource_id"], run_at=timestamp(run),
                    deadline_at=timestamp(deadline))

    def state(self, schedule, token=None):
        return self.node.call("Schedule.Get", {"id": schedule["reference"]["resource_id"]}, token)

    def wait(self, schedule, lifecycle):
        deadline = time.monotonic() + 10
        while True:
            result = self.state(schedule)
            if result["state"]["lifecycle"] != "Pending" or time.monotonic() >= deadline:
                self.assertEqual(result["state"]["lifecycle"], lifecycle, result)
                return result
            time.sleep(0.05)

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as reason:
            self.node.call(method, params, token)
        self.assertEqual(reason.exception.code, code, reason.exception)

    def budget(self, grant):
        result = self.node.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        return result["reserved"], result["spent"]

    def interval(self, grant, *, count=3, every=1, catch_up=None, overlap="Allow", **request):
        missed = {"mode": "Skip"} if catch_up is None else {"mode": "BoundedCatchUp", "limit": catch_up}
        return dict(self.request(grant, **request),
                    repeat=dict(interval_seconds=every, count=count, missed=missed, overlap=overlap))

    def triggers(self, schedule, token=None):
        reports = []
        for reference in schedule["state"]["trigger_refs"]:
            report = self.node.call("Schedule.TriggerGet", {"id": reference["resource_id"]}, token)
            self.assertEqual(report["reference"], reference)
            reports.append(report["state"])
        return reports

    def interval_progress(self, schedule, count):
        deadline = time.monotonic() + 8
        while True:
            result = self.state(schedule)
            if len(result["state"]["trigger_refs"]) >= count:
                return result
            self.assertLess(time.monotonic(), deadline, result)
            time.sleep(0.05)

    def test_restart_recovers_one_trigger_and_preserves_real_approval(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "scheduled-data"})
        grant = self.grant("volume.write", volume["resource_id"])
        request = self.request(grant, "volume.write", dict(**target(volume), name="timed", content="durable"))
        with ThreadPoolExecutor(max_workers=4) as callers:
            created = list(callers.map(lambda _: n.call("Schedule.Create", request), range(4)))
        initial = created[0]
        self.assertEqual(created, [initial] * 4)
        self.assertEqual(initial["state"]["lifecycle"], "Pending")
        self.assertEqual(initial["state"]["timezone"], "UTC")
        self.assertEqual(self.budget(grant), (0, 0))
        prepared = n.get(initial["reference"]["resource_id"])["resource"]["spec"]["request"]
        self.rejected("PermissionDenied", "Task.Submit", prepared)
        self.rejected("IdempotencyConflict", "Schedule.Create", dict(request, input=dict(request["input"], content="changed")))
        n.stop(signal.SIGKILL)
        after(request["run_at"])
        n.start()
        triggered = self.wait(initial, "Triggered")
        receipt = triggered["state"]["task_receipt"]
        task = receipt["task_id"]
        self.assertEqual(receipt["origin_ref"]["origin_type"], "Schedule")
        self.assertEqual(receipt["origin_ref"]["origin_id"], initial["reference"]["resource_id"])
        self.assertEqual(receipt["idempotency_key"], initial["state"]["trigger_id"])
        self.assertEqual(n.wait(task)["lifecycle"], "WaitingApproval")
        self.assertEqual(self.budget(grant), (1, 0))
        data = n.state / "volumes" / volume["resource_id"] / "data" / "timed"
        self.assertFalse(data.exists())
        n.stop(signal.SIGKILL)
        n.start()
        offset = timezone(timedelta(hours=5, minutes=30))
        repeated = dict(request, run_at=datetime.fromisoformat(request["run_at"]).astimezone(offset).isoformat())
        self.assertEqual(n.call("Schedule.Create", repeated), triggered)
        self.rejected("Conflict", "Schedule.Cancel", target(triggered["reference"]))
        n.approve(task)
        self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
        self.assertEqual(data.read_text(), "durable")
        self.assertEqual(self.budget(grant), (0, 1))
        self.assertEqual(len(n.call("Resource.List", {"kind": "Task"})["items"]), 2)

    def test_scope_revocation_conditional_cancel_and_missed_deadline(self):
        n = self.node
        grant = self.grant()
        request = self.request(grant, run=2, deadline=5)
        self.rejected("PermissionDenied", "Schedule.Create", dict(request, delegation_id="owner-delegation"))
        self.rejected("PermissionDenied", "Schedule.Create", dict(request, tool="node.inventory"))
        self.rejected("InvalidArgument", "Schedule.Create", dict(request, deadline_at=timestamp(90)))
        cancelled = n.call("Schedule.Create", request)
        self.rejected("Conflict", "Schedule.Cancel", dict(target(cancelled["reference"]), expected_resource_version="r99"))
        current = n.call("Schedule.Cancel", target(cancelled["reference"]))
        self.assertEqual(current["state"]["lifecycle"], "Cancelled")
        self.assertEqual(n.call("Schedule.Create", request), current)
        revoked = n.call("Schedule.Create", self.request(grant, run=1, deadline=10))
        bootstrap = n.get(n.get("owner-delegation")["value"]["grant_id"])["reference"]
        self.rejected("PermissionDenied", "Grant.Revoke", target(bootstrap))
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        rejection = self.wait(revoked, "Rejected")
        self.assertEqual(rejection["state"]["reason"], "PermissionDenied")
        self.assertEqual(self.budget(grant), (0, 0))
        self.assertIsNone(rejection["state"]["task_receipt"])
        grant = self.grant()
        request = self.request(grant, run=1, deadline=1.6)
        missed = n.call("Schedule.Create", request)
        n.stop(signal.SIGKILL)
        after(request["deadline_at"])
        n.start()
        self.assertEqual(self.wait(missed, "Expired")["state"]["reason"], "DeadlineExceeded")
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])
        self.assertEqual(self.budget(grant), (0, 0))

    def test_queue_pressure_and_cordon_hold_the_trigger_without_spending(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "admission-gate"})
        filler = n.submit("volume.write", dict(**target(volume), name="held", content="hold"))
        self.assertEqual(n.wait(filler)["lifecycle"], "WaitingApproval")
        grant = self.grant()
        request = self.request(grant, run=1)
        scheduled = n.call("Schedule.Create", request)
        self.rejected("QueueFull", "Schedule.Create", self.request(grant))
        after(request["run_at"], 0.5)
        self.assertEqual(self.state(scheduled), scheduled)
        self.assertEqual(self.budget(grant), (0, 0))
        node = n.call("Node.Get", {})["reference"]
        cordoned = n.call("Node.Configure", dict(scheduling="Cordoned", expected_resource_version=node["version"]))
        n.call("Task.Cancel", target(n.get(filler)["reference"]))
        time.sleep(0.4)
        self.assertEqual(self.state(scheduled), scheduled)
        n.call("Node.Configure", dict(scheduling="Enabled", expected_resource_version=cordoned["version"]))
        triggered = self.wait(scheduled, "Triggered")
        self.assertEqual(n.wait(triggered["state"]["task_receipt"]["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(self.budget(grant), (0, 1))

    def test_agent_uses_its_own_scope_and_receipt(self):
        n = self.node
        agent = n.call("Agent.Register", {"name": "scheduled-inventory"})
        observer, _ = n.agent(["task.read"])
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        grant = self.grant("node.inventory", node, principal=agent["principal_id"])
        request = self.request(grant, "node.inventory")
        scheduled = n.call("Schedule.Create", request, agent["token"])
        identity = {"id": scheduled["reference"]["resource_id"]}
        self.assertEqual(n.call("Schedule.Get", identity, agent["token"]), scheduled)
        self.rejected("PermissionDenied", "Schedule.Get", identity, observer["token"])
        receipt = self.wait(scheduled, "Triggered")["state"]["task_receipt"]
        self.assertEqual(receipt["origin_ref"]["authority_id"], node)
        task = n.wait(receipt["task_id"])
        observed = n.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, agent["token"])
        self.assertGreater(observed["memory_bytes"]["MemTotal"], 0)
        self.rejected("PermissionDenied", "Task.Get", {"id": task["task_id"]}, observer["token"])

    def test_owner_queue_pressure_does_not_block_a_later_timed_schedule(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=4,
                                            principal_queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "timed-schedule-fairness"})
        held = n.submit("volume.write", {**target(volume), "name": "held", "content": "pending"})
        self.assertEqual(n.wait(held)["lifecycle"], "WaitingApproval")

        blocked_scope = self.grant("volume.write", volume["resource_id"], units=2)
        independent = n.call("Agent.Register", {"name": "independent-timed-schedule"})
        independent_scope = self.grant(principal=independent["principal_id"])
        blocked = n.call("Schedule.Create", self.interval(
            blocked_scope, tool="volume.write", count=2, every=20, run=0.5, deadline=25,
            input={**target(volume), "name": "original", "content": "scheduled once"},
        ))
        later = n.call("Schedule.Create", self.request(independent_scope, run=1.2), independent["token"])
        later_receipt = self.wait(later, "Triggered")["state"]["task_receipt"]
        self.assertEqual(n.wait(later_receipt["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(self.state(blocked), blocked)
        self.assertEqual(self.budget(blocked_scope), (0, 0))
        self.assertEqual(self.budget(independent_scope), (0, 1))

        original = blocked["state"]["next"]
        blocked_id = blocked["reference"]["resource_id"]
        n.stop(signal.SIGKILL)
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            admission = conn.execute(
                "SELECT d.kind,d.trigger,d.anchor,p.run_ms,d.schedule_rev FROM schedule_admission_deferrals d "
                "JOIN pending_schedules p ON p.schedule=d.schedule WHERE d.schedule=?", (blocked_id,)
            ).fetchone()
            self.assertIsNotNone(admission)
            self.assertEqual((admission[0], admission[1], admission[4]), ("Timed", original["trigger_id"], 1))
            self.assertEqual(admission[2], admission[3])
        verification = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=20))
        self.assertGreaterEqual(verification["database_format"], 101, verification)
        with closing(sqlite3.connect(n.state / "state.db")) as conn, conn:
            conn.execute("UPDATE schedule_admission_deferrals SET trigger='swapped' WHERE schedule=?", (blocked_id,))
        invalid = subprocess.run(n.daemon + ["verify"], capture_output=True, text=True, timeout=20)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("RecoveryRequired", invalid.stderr)
        with closing(sqlite3.connect(n.state / "state.db")) as conn, conn:
            conn.execute("UPDATE schedule_admission_deferrals SET trigger=? WHERE schedule=?",
                         (original["trigger_id"], blocked_id))
        n.start()
        self.assertEqual(self.state(blocked), blocked)
        self.assertEqual(self.state(later)["state"]["task_receipt"], later_receipt)

        n.call("Task.Cancel", target(n.get(held)["reference"]))
        progress = self.interval_progress(blocked, 1)
        executed, = self.triggers(progress)
        receipt = executed["task_receipt"]
        self.assertEqual(progress["state"]["trigger_refs"][0]["resource_id"], original["trigger_id"])
        self.assertEqual(executed["run_at"], original["run_at"])
        self.assertEqual(executed["deadline_at"], original["deadline_at"])
        self.assertEqual(receipt["idempotency_key"], original["trigger_id"])
        self.assertEqual(receipt["origin_ref"]["origin_id"], blocked_id)
        child = receipt["task_id"]
        self.assertEqual(n.wait(child)["lifecycle"], "WaitingApproval")
        data = n.state / "volumes" / volume["resource_id"] / "data"
        self.assertFalse((data / "original").exists())
        n.approve(child)
        self.assertEqual(n.wait(child)["lifecycle"], "Succeeded")
        self.assertEqual((data / "original").read_text(), "scheduled once")
        self.assertEqual(self.budget(blocked_scope), (0, 1))
        with closing(sqlite3.connect(n.state / "state.db")) as conn:
            self.assertIsNone(conn.execute("SELECT 1 FROM schedule_admission_deferrals WHERE schedule=?",
                                           (blocked_id,)).fetchone())
            self.assertEqual(conn.execute("SELECT count(*) FROM submissions WHERE task=?", (child,)).fetchone(), (1,))

    def test_manual_retry_preserves_the_original_trigger_identity(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "scheduled-retry"})
        grant = self.grant("volume.write", volume["resource_id"])
        request = self.request(grant, "volume.write", dict(**target(volume), name="retry", content="once"), run=0.75)
        scheduled = n.call("Schedule.Create", request)
        original = self.wait(scheduled, "Triggered")
        task = original["state"]["task_receipt"]["task_id"]
        n.approve(task, approve=False)
        self.assertEqual(n.wait(task)["lifecycle"], "Cancelled")
        self.assertEqual(self.budget(grant), (0, 0))
        retry = n.call("Task.Retry", dict(**target(n.get(task)["reference"]), key=key()))
        self.assertEqual(retry["origin_ref"]["origin_type"], "HumanRequest")
        child = n.wait(retry["task_id"], autoapprove=True)
        self.assertEqual((child["lifecycle"], child["retry_of_task_id"]), ("Succeeded", task))
        self.assertEqual(self.state(scheduled), original)
        self.assertEqual(self.budget(grant), (0, 1))

    def test_calendar_pins_utc_and_reuses_the_original_trigger_after_restart(self):
        n = self.node
        grant = self.grant()
        due = (datetime.now(timezone.utc) + timedelta(seconds=4)).replace(microsecond=0)
        phoenix = due.astimezone(timezone(timedelta(hours=-7)))
        calendar = dict(timezone="America/Phoenix", start_date=phoenix.date().isoformat(),
                        local_time=phoenix.time().isoformat(), days=1, window_seconds=15,
                        gap="Reject", fold="Earlier", missed={"mode": "Skip"}, overlap="Allow")
        request = dict(key=key(), tool="system.status", input={}, calendar=calendar,
                       delegation_id=grant["delegation_ref"]["resource_id"])
        self.rejected("InvalidArgument", "Schedule.Create", dict(request, run_at=timestamp(5)))
        self.rejected("InvalidArgument", "Schedule.Create",
                      dict(request, calendar=dict(calendar, timezone="Unregistered/Timezone")))
        self.rejected("InvalidArgument", "Schedule.Create",
                      dict(request, calendar=dict(calendar, window_seconds=300)))
        initial = n.call("Schedule.Create", request)
        state = initial["state"]
        self.assertEqual(state["timezone"], "America/Phoenix")
        self.assertEqual(state["calendar"], calendar)
        plan = state["calendar_plan"]
        self.assertTrue(plan["timezone_database"])
        self.assertEqual(plan["skipped_dates"], [])
        window, = plan["windows"]
        self.assertEqual(datetime.fromisoformat(window["run_at"]), due)
        self.assertEqual(datetime.fromisoformat(window["deadline_at"]), due + timedelta(seconds=15))
        self.rejected("IdempotencyConflict", "Schedule.Create",
                      dict(request, calendar=dict(calendar, fold="Later")))
        n.stop(signal.SIGKILL)
        after(window["run_at"])
        n.start()
        completed = self.wait(initial, "Completed")
        self.assertEqual(completed["state"]["calendar_plan"], plan)
        trigger, = self.triggers(completed)
        self.assertEqual(trigger["ordinal"], 1)
        self.assertEqual(trigger["lifecycle"], "Triggered", trigger)
        receipt = trigger["task_receipt"]
        self.assertEqual(receipt["idempotency_key"], state["next"]["trigger_id"])
        self.assertEqual(n.wait(receipt["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(n.call("Schedule.Create", request), completed)
        self.assertEqual(self.budget(grant), (0, 1))

    def test_interval_recovers_only_the_bounded_tail_and_persists_distinct_receipts(self):
        n = self.node
        grant = self.grant(units=2)
        request = self.interval(grant, count=5, catch_up=2, run=1.2, deadline=15)
        schedule = n.call("Schedule.Create", request)
        self.assertEqual(schedule["state"]["next"]["ordinal"], 1)
        self.assertEqual(self.triggers(schedule), [])
        self.assertEqual(self.budget(grant), (0, 0))
        self.rejected("IdempotencyConflict", "Schedule.Create",
                      dict(request, repeat=dict(request["repeat"], count=4)))
        n.stop(signal.SIGKILL)
        after(request["run_at"], 4.15)
        n.start()
        settled = self.wait(schedule, "Completed")
        self.assertIsNone(settled["state"]["next"])
        reports = self.triggers(settled)
        self.assertEqual([r["ordinal"] for r in reports], list(range(1, 6)))
        self.assertEqual([r["lifecycle"] for r in reports], ["Skipped"] * 3 + ["Triggered"] * 2)
        self.assertEqual([r["reason"] for r in reports], ["CatchUpLimit"] * 3 + [None, None])
        start = datetime.fromisoformat(request["run_at"])
        end = datetime.fromisoformat(request["deadline_at"])
        for index, report in enumerate(reports):
            offset = timedelta(seconds=index)
            self.assertEqual(datetime.fromisoformat(report["run_at"]), start + offset)
            self.assertEqual(datetime.fromisoformat(report["deadline_at"]), end + offset)
            self.assertEqual(report["schedule_id"], schedule["reference"]["resource_id"])
            if index < 3:
                self.assertIsNone(report["task_receipt"])
        references = settled["state"]["trigger_refs"]
        self.assertEqual(len({r["resource_id"] for r in references}), 5)
        self.assertEqual(references[0]["resource_id"], schedule["state"]["next"]["trigger_id"])
        tasks = []
        for reference, report in zip(references[3:], reports[3:], strict=True):
            receipt = report["task_receipt"]
            self.assertEqual(receipt["origin_ref"]["origin_type"], "Schedule")
            self.assertEqual(receipt["origin_ref"]["origin_id"], schedule["reference"]["resource_id"])
            self.assertEqual(receipt["idempotency_key"], reference["resource_id"])
            self.assertEqual(n.wait(receipt["task_id"])["lifecycle"], "Succeeded")
            tasks.append(receipt["task_id"])
        self.assertEqual(len(set(tasks)), 2)
        self.assertEqual(self.budget(grant), (0, 2))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Schedule.Create", request), settled)
        self.assertEqual(self.triggers(settled), reports)
        self.assertEqual(len(n.call("Resource.List", {"kind": "Task"})["items"]), 2)

    def test_interval_skips_overlap_and_cancels_only_future_triggers(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "no-schedule-overlap"})
        grant = self.grant("volume.write", volume["resource_id"], units=3)
        request = self.interval(grant, every=2, overlap="Skip", run=1.2, tool="volume.write",
                                input=dict(**target(volume), name="first", content="once"))
        schedule = n.call("Schedule.Create", request)
        first, = self.triggers(self.interval_progress(schedule, 1))
        task = first["task_receipt"]["task_id"]
        self.assertEqual(n.wait(task)["lifecycle"], "WaitingApproval")
        current = self.interval_progress(schedule, 2)
        first, second = self.triggers(current)
        self.assertEqual((second["lifecycle"], second["reason"]), ("Skipped", "PreviousTaskActive"))
        self.assertEqual(self.budget(grant), (1, 0))
        self.rejected("Conflict", "Schedule.Cancel", target(schedule["reference"]))
        cancelled = n.call("Schedule.Cancel", target(current["reference"]))
        self.assertEqual(cancelled["state"]["lifecycle"], "Cancelled")
        self.assertIsNone(cancelled["state"]["next"])
        n.approve(task)
        self.assertEqual(n.wait(task)["lifecycle"], "Succeeded")
        after(request["run_at"], 4.4)
        self.assertEqual(self.state(schedule), cancelled)
        self.assertEqual(self.triggers(cancelled), [first, second])
        self.assertEqual(self.budget(grant), (0, 1))

    def test_interval_expires_missed_windows_and_keeps_agent_receipts_scoped(self):
        n = self.node
        agent = n.call("Agent.Register", {"name": "recurring-agent"})
        observer, _ = n.agent(["task.read"])
        node = n.call("Node.Get", {})["reference"]["resource_id"]
        grant = self.grant("node.inventory", node, principal=agent["principal_id"], units=3)
        request = self.interval(grant, count=4, run=1.2, deadline=4, tool="node.inventory")
        for invalid in [{"count": 33}, {"interval_seconds": 0}, {"interval_seconds": 60},
                        {"missed": {"mode": "BoundedCatchUp", "limit": 9}}]:
            self.rejected("InvalidArgument", "Schedule.Create",
                          dict(request, repeat=dict(request["repeat"], **invalid)), agent["token"])
        schedule = n.call("Schedule.Create", request, agent["token"])
        n.stop(signal.SIGKILL)
        after(request["run_at"], 3.25)
        n.start()
        settled = self.wait(schedule, "Completed")
        reports = self.triggers(settled, agent["token"])
        self.assertEqual((reports[0]["lifecycle"], reports[0]["reason"]), ("Expired", "DeadlineExceeded"))
        self.assertEqual([r["reason"] for r in reports[1:3]], ["MissedTrigger", "MissedTrigger"])
        self.assertEqual(reports[3]["lifecycle"], "Triggered")
        trigger = settled["state"]["trigger_refs"][-1]["resource_id"]
        self.rejected("PermissionDenied", "Schedule.TriggerGet", {"id": trigger}, observer["token"])
        self.rejected("PermissionDenied", "Resource.Get", {"id": trigger}, agent["token"])
        task = n.wait(reports[3]["task_receipt"]["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded")
        self.assertEqual(self.budget(grant), (0, 1))

    def test_interval_revocation_settles_one_rejection_and_closes_future_work(self):
        n = self.node
        grant = self.grant(units=3)
        request = self.interval(grant, run=1.2, every=2)
        schedule = n.call("Schedule.Create", request)
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        rejected = self.wait(schedule, "Rejected")
        self.assertIsNone(rejected["state"]["next"])
        trigger, = self.triggers(rejected)
        self.assertEqual((trigger["lifecycle"], trigger["reason"], trigger["task_receipt"]),
                         ("Rejected", "PermissionDenied", None))
        after(request["run_at"], 2.4)
        self.assertEqual(self.state(schedule), rejected)
        self.assertEqual(self.budget(grant), (0, 0))
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])

    def test_interval_retries_queue_admission_using_the_next_trigger_identity(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), queue_limit=1)))
        n.start()
        volume = n.perform("volume.create", {"name": "interval-queue-gate"})
        filler = n.submit("volume.write", dict(**target(volume), name="held", content="hold"))
        self.assertEqual(n.wait(filler)["lifecycle"], "WaitingApproval")
        grant = self.grant(units=2)
        request = self.interval(grant, every=2, catch_up=1, run=1.2)
        schedule = n.call("Schedule.Create", request)
        current = self.interval_progress(schedule, 1)
        self.assertEqual(current["state"]["next"]["ordinal"], 2)
        dropped, = self.triggers(current)
        self.assertEqual((dropped["lifecycle"], dropped["reason"]), ("Skipped", "CatchUpLimit"))
        self.assertEqual(self.budget(grant), (0, 0))
        through_queue = current["state"]["next"]["trigger_id"]
        n.call("Task.Cancel", target(n.get(filler)["reference"]))
        final = self.wait(schedule, "Completed")
        second, third = self.triggers(final)[1:]
        self.assertEqual(second["task_receipt"]["idempotency_key"], through_queue)
        for report in [second, third]:
            self.assertEqual(n.wait(report["task_receipt"]["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(self.budget(grant), (0, 2))

    def test_admitted_schedule_expiry_settles_while_capacity_blocks_new_tasks(self):
        n = self.node
        grant = self.grant(units=3)
        view = n.call("Node.Get", {})["authority_storage"]["state"]
        reserve = int(view["capacity_bytes"]) + 4096
        admission = n.prepare("system.status", {})
        once_request = self.request(grant, run=1.5, deadline=3)
        timer = n.call("Schedule.Create", once_request)
        interval_request = self.interval(grant, count=2, every=1, run=1.5, deadline=2.8)
        series = n.call("Schedule.Create", interval_request)
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()), authority_reserve={"state_bytes": reserve})))
        n.start()
        self.assertTrue(n.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        self.rejected("QuotaExceeded", "Task.Submit", admission)
        complete_once = self.wait(timer, "Expired")
        self.assertEqual(complete_once["state"]["reason"], "DeadlineExceeded")
        self.assertIsNone(complete_once["state"]["task_receipt"])
        complete_series = self.wait(series, "Completed")
        reports = self.triggers(complete_series)
        self.assertEqual([r["ordinal"] for r in reports], [1, 2])
        self.assertTrue(all(r["lifecycle"] == "Expired" and r["task_receipt"] is None for r in reports))
        self.assertEqual(self.budget(grant), (0, 0))
        self.assertEqual(n.call("Resource.List", {"kind": "Task"})["items"], [])
        n.stop()
        n.start()
        self.assertEqual(n.call("Schedule.Create", once_request), complete_once)
        self.assertEqual(n.call("Schedule.Create", interval_request), complete_series)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION"), "requires a bounded filesystem")
    def test_waits_for_authority_filesystem_headroom(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()),
                                            authority_reserve={"state_bytes": 4 * 1024 * 1024})))
        n.start()
        grant = self.grant()
        request = self.request(grant, run=1)
        schedule = n.call("Schedule.Create", request)
        storage = n.call("Node.Get", {})["authority_storage"]["state"]
        self.assertLessEqual(int(storage["capacity_bytes"]), 64 * 1024 * 1024)
        scratch = n.base / "fill"
        with scratch.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, int(storage["available_bytes"]) - 2 * 1024 * 1024)
        self.assertTrue(n.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        after(request["run_at"], 0.5)
        self.assertEqual(self.state(schedule), schedule)
        self.assertEqual(self.budget(grant), (0, 0))
        scratch.unlink()
        triggered = self.wait(schedule, "Triggered")
        self.assertEqual(n.wait(triggered["state"]["task_receipt"]["task_id"])["lifecycle"], "Succeeded")
