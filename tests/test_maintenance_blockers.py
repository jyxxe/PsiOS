"""Inspect actual task and storage custody at the local Unix maintenance gate."""
from datetime import datetime
import json
import os
import signal
import subprocess
from time import monotonic, sleep
import unittest

from support import Failure, Node, target
from test_recovery_admission import lose_restore_receipt


GET = "Node.WorkAdmission.Get"
SET = "Node.WorkAdmission.Set"
BLOCKERS = "Node.WorkAdmission.Blockers"
EMPTY_PAGE = {"items": [], "truncated": False}


class MaintenanceBlockersTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as caught:
            self.node.call(method, params, token)
        self.assertEqual(caught.exception.code, code, caught.exception)

    def grant(self, capability, resources):
        return self.node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["task.submit", "task.read", capability],
            "tool_targets": resources, "units": 8, "ttl_seconds": 120,
        })["delegation_ref"]["resource_id"]

    def scheduling(self, mode):
        n = self.node
        revision = n.call("Node.Get", {})["reference"]["version"]
        n.call("Node.Configure", {"expected_resource_version": revision, "scheduling": mode})

    def blockers(self, admission, **options):
        query = {"expected_resource_version": admission["version"], **options}
        view = self.node.call(BLOCKERS, query)
        self.assertEqual(view["reference"], admission, view)
        self.assertIsNotNone(datetime.fromisoformat(view["observed_at"]).utcoffset(), view)
        for kind in ("tasks", "exclusive_holds", "volume_read_holds", "model_leases", "model_outcomes_unknown"):
            page = view[kind]
            self.assertEqual(set(page), {"items", "truncated"}, page)
            self.assertIsInstance(page["truncated"], bool, page)
            self.assertLessEqual(len(page["items"]), options.get("limit", 64), page)
        return view

    def accepted(self, tool, params, delegation):
        request = self.node.prepare(tool, params, delegation=delegation)
        return self.node.call("Task.Submit", request)["task_id"]

    def task_refs(self, task_id):
        n = self.node
        queued = n.get(task_id)
        operation_ref, = queued["value"]["operation_refs"]
        operation = n.get(operation_ref["resource_id"])
        self.assertEqual(operation["reference"], operation_ref, operation)
        return queued, operation

    def assert_task_blocker(self, row, task, phase, recovery=False):
        queued, operation = self.task_refs(task)
        self.assertIs(row["recovery_admission"], recovery, row)
        self.assertEqual(row, {
            "phase": phase, "task_ref": queued["reference"], "operation_ref": operation["reference"],
            "operation_lifecycle": operation["value"]["lifecycle"],
            "outcome_knowledge": operation["value"]["outcome_knowledge"],
            "recovery_admission": recovery,
        })

    def assert_empty(self, view):
        for kind in ("tasks", "exclusive_holds", "volume_read_holds", "model_leases", "model_outcomes_unknown"):
            self.assertEqual(view[kind], EMPTY_PAGE, view)

    def verify(self):
        self.node.stop()
        report = json.loads(subprocess.check_output([*self.node.daemon, "verify"], timeout=20))
        self.assertIs(report["migration_required"], False, report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"], report["witness"]),
                         ("Verified", "Verified", "Confirmed"), report)

    def test_unknown_holds_and_recovery_tasks_project_exact_refs_and_clear_after_reconcile(self):
        n = self.node
        original = n.call(GET, {})["reference"]
        self.assert_empty(self.blockers(original))
        outsider, _ = n.agent(["task.read", "resource.read"])
        self.denied("PermissionDenied", BLOCKERS, {"expected_resource_version": original["version"]},
                    outsider["token"])
        for limit in (0, 65, -1, True, "1"):
            self.denied("InvalidArgument", BLOCKERS, {"expected_resource_version": original["version"], "limit": limit})

        volume = n.perform("volume.create", {"name": "maintenance-blocker-source"})
        content = b"original volume data survives restore acknowledgement loss\n"
        (n.state / "volumes" / volume["resource_id"] / "data" / "record").write_bytes(content)
        unresolved = []
        for _ in range(2):
            backup = n.perform("backup.create", target(volume))
            task, paused = lose_restore_receipt(n, backup)
            self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
            operation, = paused["operation_refs"]
            restored = n.state / "volumes" / f"volume-{operation['resource_id']}" / "data" / "record"
            self.assertEqual(restored.read_bytes(), content)
            unresolved.append((backup, task, operation, restored, restored.stat().st_ino))
        self.assertNotEqual(unresolved[0][0], unresolved[1][0])

        self.scheduling("Cordoned")
        ordinary = n.submit("system.status", {})
        awaiting = n.submit("volume.write", {**target(volume), "name": "not-approved", "content": "never"})
        self.assertEqual(n.wait(awaiting)["lifecycle"], "WaitingApproval")
        gate = n.call(SET, {"expected_resource_version": original["version"], "mode": "Draining",
                            "reason": "inspect exact unresolved storage work"})["reference"]
        self.denied("Conflict", BLOCKERS, {"expected_resource_version": original["version"]})
        tool = "volume.inspect_publication"
        delegation = self.grant(tool, [operation["resource_id"] for _, _, operation, _, _ in unresolved])
        recovery = [self.accepted(tool, target(operation), delegation) for _, _, operation, _, _ in unresolved]

        full = self.blockers(gate, limit=64)
        task_page = full["tasks"]
        self.assertFalse(task_page["truncated"], task_page)
        rows = task_page["items"]
        tasks = {row["task_ref"]["resource_id"]: row for row in rows}
        ranked = sorted(task for _, task, _, _, _ in unresolved) + [awaiting] + sorted([ordinary, *recovery])
        self.assertEqual([row["task_ref"]["resource_id"] for row in rows], ranked, rows)
        for _, task, _, _, _ in unresolved:
            self.assert_task_blocker(tasks[task], task, "Unknown")
            self.assertEqual(tasks[task]["outcome_knowledge"], "Unknown", tasks[task])
        self.assert_task_blocker(tasks[ordinary], ordinary, "Queued")
        self.assert_task_blocker(tasks[awaiting], awaiting, "WaitingApproval")
        for task in recovery:
            self.assert_task_blocker(tasks[task], task, "Queued", recovery=True)

        for task, (_, _, operation, _, _) in zip(recovery, unresolved):
            self.assertEqual(n.get(f"recovery-admission-{task}")["value"]["unresolved_operation_ref"], operation)

        holds = full["exclusive_holds"]
        self.assertFalse(holds["truncated"], holds)
        ordered_holds = sorted(unresolved, key=lambda restore: restore[0]["resource_id"])
        self.assertEqual(holds["items"], [{
            "resource_kind": "Backup", "resource_ref": backup, "holder_kind": "Operation",
            "holder_ref": operation, "task_ref": tasks[task]["task_ref"],
        } for backup, task, operation, _, _ in ordered_holds])
        self.assertEqual(full["volume_read_holds"], EMPTY_PAGE, full)
        one = self.blockers(gate, limit=1)
        for group in ("tasks", "exclusive_holds"):
            self.assertEqual(one[group], {"items": full[group]["items"][:1], "truncated": True}, one)
        for group in ("volume_read_holds", "model_leases", "model_outcomes_unknown"):
            self.assertEqual(one[group], EMPTY_PAGE, one)
        cli = n.cli(n.witness / "owner.token", "node-work-admission-blockers", gate["version"], "--limit", "1")
        self.assertEqual({k: v for k, v in cli.items() if k != "observed_at"},
                         {k: v for k, v in one.items() if k != "observed_at"})

        n.stop(signal.SIGKILL)
        n.start()
        persisted = self.blockers(gate)
        self.assertEqual({k: v for k, v in persisted.items() if k != "observed_at"},
                         {k: v for k, v in full.items() if k != "observed_at"})
        for task in (ordinary, awaiting):
            n.call("Task.Cancel", target(n.get(task)["reference"]))
            self.assertEqual(n.wait(task)["lifecycle"], "Cancelled")
        self.scheduling("Enabled")
        for task, (_, source, operation, path, inode) in zip(recovery, unresolved):
            observed = n.wait(task)
            self.assertEqual(observed["lifecycle"], "Succeeded", observed)
            evidence, = observed["result"]["artifact_refs"]
            receipt = n.call("Operation.Reconcile", {**target(operation), "evidence_ref": evidence})
            self.assertEqual(receipt["outcome_knowledge"], "Known", receipt)
            self.assertEqual(n.wait(source)["lifecycle"], "Succeeded")
            self.assertEqual(path.stat().st_ino, inode)
            self.assertEqual(path.read_bytes(), content)
        self.assertFalse((n.state / "volumes" / volume["resource_id"] / "data" / "not-approved").exists())
        self.assert_empty(self.blockers(gate))
        self.verify()

    def test_live_scoped_volume_reader_reports_the_same_task_and_operation_cut(self):
        n = self.node
        volume = n.perform("volume.create", {"name": "maintenance-read-hold"})
        path = n.state / "volumes" / volume["resource_id"] / "data" / "payload"
        size = 256 * 1024 * 1024
        with path.open("wb") as payload:
            payload.write(b"one real retained file\n")
            payload.truncate(size)
        tool = "volume.verify_file"
        delegation = self.grant(tool, [volume["resource_id"]])
        gate = n.call(GET, {})["reference"]
        report = None
        # The read hold exists only during the actual check. Poll the Unix view
        # and allow another attempt if the host finishes before a poll runs.
        for _ in range(3):
            task = self.accepted(tool, {"target": target(volume), "path": path.name, "max_bytes": size}, delegation)
            deadline = monotonic() + 15
            while monotonic() < deadline:
                view = self.blockers(gate)
                candidates = [row for row in view["volume_read_holds"]["items"]
                              if row["task_ref"]["resource_id"] == task]
                if candidates:
                    report = view
                    break
                if n.call("Task.Get", {"id": task})["lifecycle"] in ("Succeeded", "Failed", "Cancelled"):
                    break
                sleep(0.003)
            result = n.wait(task)
            self.assertEqual((result["lifecycle"], result["outcome_knowledge"]), ("Succeeded", "Known"), result)
            if report is not None:
                break
        self.assertIsNotNone(report, "no live volume read hold observed while hashing the real file")
        read, = report["volume_read_holds"]["items"]
        blocker, = report["tasks"]["items"]
        self.assertEqual(read, {"volume_ref": volume, "task_ref": blocker["task_ref"],
                                "operation_ref": blocker["operation_ref"]}, report)
        self.assertEqual((blocker["phase"], blocker["outcome_knowledge"]), ("Dispatched", "InFlight"), blocker)
        self.assertEqual(n.call("Resource.Resolve", {"reference": read["task_ref"]})["value"]["lifecycle"],
                         "Running")
        self.assertEqual(n.call("Resource.Resolve", {"reference": read["operation_ref"]})["value"]["lifecycle"],
                         blocker["operation_lifecycle"])
        self.assertEqual(report["exclusive_holds"], EMPTY_PAGE, report)
        artifact, = result["result"]["artifact_refs"]
        proof = n.call("Artifact.Read", {"reference": artifact})
        self.assertEqual(proof["volume_ref"], volume, proof)
        self.assertEqual(proof["revision"]["inode"], str(path.stat().st_ino), proof)
        self.assertEqual(proof["revision"]["size_bytes"], str(size), proof)
        self.assert_empty(self.blockers(gate))
        self.verify()


if __name__ == "__main__":
    unittest.main()
