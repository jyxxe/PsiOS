"""Verify an explicit network trial rollback and recovery after its receipt is lost.

Run with sudo unshare --net --mount --uts --propagation private \
    python3 -B tests/network_trial_abort.py --bin-dir target/debug
"""
import argparse
from contextlib import closing, contextmanager
import json
import re
import signal
import sqlite3
import subprocess

from network_manager_trial import CANDIDATE, NM, ORIGINAL, boottime, child, isolated_manager, until
from network_trial import (
    CONFIRMATION_SECONDS, INSPECT, TRIAL, approved_trial, artifact, check_contender, rejects,
    replay, scope, waiting,
)
from support import Node, target


ROLLBACK = "NetworkTrial.Rollback"
RECEIPT = "NetworkTrialRollbackReceipt"


def method_calls(log, member):
    records = re.split(r"(?m)^[^\n]*\bType=", log.read_text(errors="replace"))[1:]
    member_field = re.compile(rf"\bMember={re.escape(member)}(?=\s|$)")
    return [record for record in records if record.startswith("method_call ") and member_field.search(record)]


@contextmanager
def monitor(manager):
    log = manager.root / "rollback-monitor.log"
    command = [
        "stdbuf", "-oL", "busctl", f"--address={manager.bus_address}", "--no-pager",
        f"--match=type='method_call',interface='{NM}'", "monitor",
    ]
    environment = {**manager.environment, "SYSTEMD_COLORS": "0"}
    with (
        log.open("w") as output,
        child(command, stdout=output, stderr=subprocess.STDOUT, env=environment) as process,
    ):
        def ready():
            assert process.poll() is None, log.read_text()
            manager.manager_call("GetDeviceByIpIface", "s", manager.interface)
            return bool(method_calls(log, "GetDeviceByIpIface"))

        until("private bus monitor observes a harmless query", ready, True)
        yield log


def rollback_calls(log, checkpoints):
    def paths():
        calls = method_calls(log, "CheckpointRollback")
        return [re.findall(r'\bOBJECT_PATH "([^"]+)";', call) for call in calls]

    until("private bus saw exactly the expected rollback paths", paths, [[item.path] for item in checkpoints],
          timeout=4)
    assert not method_calls(log, "CheckpointDestroy")
    assert not method_calls(log, "CheckpointAdjustRollbackTimeout")


def install_receipt_fault(node):
    node.stop()
    with closing(sqlite3.connect(node.state / "state.db")) as db, db:
        db.execute(f"""
            CREATE TRIGGER fail_network_rollback_receipt BEFORE INSERT ON versions
            WHEN NEW.kind='{RECEIPT}' BEGIN
                SELECT RAISE(ABORT, 'fixture lost the acknowledged rollback receipt');
            END
        """)
    node.start()


def remove_receipt_fault_after_crash(node, operation, link, checkpoint):
    node.stop(signal.SIGKILL)
    operation_id = operation["resource_id"]
    with closing(sqlite3.connect(node.state / "state.db")) as db, db:
        journal = db.execute("""
            SELECT body->>'phase', body->'checkpoint'->>'path' FROM current
              WHERE kind='NetworkTrialJournal' AND spec->>'operation_id'=?
        """, (operation_id,)).fetchone()
        assert journal == ("RollbackIssued", checkpoint.path), journal
        assert db.execute("SELECT count(*) FROM current WHERE kind=? AND spec->>'operation_id'=?",
                          (RECEIPT, operation_id)).fetchone() == (0,)
        assert db.execute("SELECT holder FROM holds WHERE volume=?", (link["resource_id"],)).fetchone() == (
            operation_id,
        )
        db.execute("DROP TRIGGER fail_network_rollback_receipt")
    node.start()


def assert_work_blocker(node, task_id, operation, link, phase, knowledge):
    admission = node.call("Node.WorkAdmission.Get", {})["reference"]
    blockers = node.call("Node.WorkAdmission.Blockers", {
        "expected_resource_version": admission["version"], "limit": 8,
    })
    task_ref = node.get(task_id)["reference"]
    entry, = [item for item in blockers["tasks"]["items"] if item["task_ref"] == task_ref]
    assert (entry["phase"], entry["operation_ref"], entry["outcome_knowledge"]) == (
        phase, operation, knowledge,
    ), entry
    hold, = [item for item in blockers["exclusive_holds"]["items"] if item["resource_ref"] == link]
    assert hold == {
        "resource_kind": "NetworkLink", "resource_ref": link,
        "holder_kind": "Operation", "holder_ref": operation, "task_ref": task_ref,
    }, hold


def exercise(node, manager):
    node.stop()
    config = json.loads(node.config.read_text())
    config.update(workers=1, network_link={
        "interface": manager.interface, "mac": manager.mac,
        "original_uuid": ORIGINAL.uuid, "candidate_uuid": CANDIDATE.uuid,
        "confirmation_seconds": CONFIRMATION_SECONDS,
    })
    node.config.chmod(0o600)
    node.config.write_text(json.dumps(config))
    node.start()

    owner_token = node.witness / "owner.token"
    link_id = "network-link-" + manager.mac.replace(":", "")
    outsider, _ = node.agent(["task.read"])
    delegation = scope(node, TRIAL, link_id)
    link = node.call("NetworkLink.Get", {"id": link_id})["reference"]
    with monitor(manager) as log:
        before, first = approved_trial(node, link, delegation)
        waiting_operation = waiting(node, first)
        assert_work_blocker(node, first, waiting_operation, link, "WaitingInput", "InFlight")
        checkpoint = manager.only_checkpoint(CONFIRMATION_SECONDS)
        manager.assert_profile(CANDIDATE)
        challenge = node.call("NetworkTrial.Confirmation", target(waiting_operation))
        rejects("PermissionDenied", node, ROLLBACK, target(waiting_operation), outsider["token"])
        rejects("Conflict", node, ROLLBACK, {**target(waiting_operation), "expected_resource_version": "r1"})
        assert manager.checkpoints() == [checkpoint.path]
        assert not method_calls(log, "CheckpointRollback")
        assert boottime() < checkpoint.deadline

        rolled_back = node.cli(owner_token, "network-trial-rollback", waiting_operation["resource_id"],
                               "--revision", waiting_operation["version"])
        assert (rolled_back["lifecycle"], rolled_back["outcome_knowledge"]) == ("Cancelled", "Known"), rolled_back
        manager.assert_profile(ORIGINAL)
        assert manager.checkpoints() == [] and boottime() < checkpoint.deadline, checkpoint
        rollback_calls(log, [checkpoint])
        settled = node.wait(first)
        assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Cancelled", "Known"), settled
        operation, = settled["operation_refs"]
        assert operation["resource_id"] == waiting_operation["resource_id"] and operation != waiting_operation
        assert node.get(operation["resource_id"])["value"] == rolled_back
        result_ref, result = artifact(node, settled)
        assert result == {"publication": "RolledBack", "link_ref": link}, result
        rejects("Conflict", node, ROLLBACK, target(waiting_operation))
        rejects("PreconditionFailed", node, ROLLBACK, target(operation))
        rejects("PreconditionFailed", node, "NetworkTrial.Confirm", {
            **target(operation), "confirmation_token": challenge["confirmation_token"],
        })
        replay(node, before, first)

        install_receipt_fault(node)
        assert node.call("Artifact.Read", {"reference": result_ref}) == result
        second_request, second = approved_trial(node, link, delegation)
        pending = waiting(node, second)
        second_checkpoint = manager.only_checkpoint(CONFIRMATION_SECONDS)
        assert checkpoint.path != second_checkpoint.path
        manager.assert_profile(CANDIDATE)

        # The real manager applies CheckpointRollback, but the committed issue survives without its receipt.
        rejects("StorageUnavailable", node, ROLLBACK, target(pending))
        manager.assert_profile(ORIGINAL)
        assert manager.checkpoints() == [] and boottime() < second_checkpoint.deadline, second_checkpoint
        rollback_calls(log, [checkpoint, second_checkpoint])
        unsettled = node.call("Task.Get", {"id": second})
        assert (unsettled["lifecycle"], unsettled["outcome_knowledge"]) == ("WaitingInput", "InFlight"), unsettled
        assert not unsettled["result"]["artifact_refs"], unsettled
        rejects("PreconditionFailed", node, ROLLBACK, target(pending))

        remove_receipt_fault_after_crash(node, pending, link, second_checkpoint)
        paused = node.wait(second)
        assert (paused["lifecycle"], paused["outcome_knowledge"], paused["conditions"][0]["type"]) == (
            "Paused", "Unknown", "OutcomeUnknown",
        ), paused
        unresolved, = paused["operation_refs"]
        assert unresolved["resource_id"] == pending["resource_id"] and unresolved != pending
        assert_work_blocker(node, second, unresolved, link, "Unknown", "Unknown")
        rejects("Conflict", node, ROLLBACK, target(pending))
        rejects("PreconditionFailed", node, ROLLBACK, target(unresolved))
        check_contender(node, link, delegation)
        replay(node, second_request, second)
        rollback_calls(log, [checkpoint, second_checkpoint])

        until("the original native rollback deadline elapses", lambda: boottime() > second_checkpoint.deadline,
              True, timeout=CONFIRMATION_SECONDS + 6)
        inspected = node.prepare(INSPECT, target(unresolved),
                                 delegation=scope(node, INSPECT, unresolved["resource_id"]))
        inspection = node.wait(node.call("Task.Submit", inspected)["task_id"])
        assert (inspection["lifecycle"], inspection["outcome_knowledge"]) == ("Succeeded", "Known"), inspection
        proof_ref, proof = artifact(node, inspection)
        assert (proof["operation_ref"], proof["link_ref"], proof["publication"]) == (
            unresolved, link, "RolledBack",
        ), proof
        assert proof["observed"]["checkpoint_present"] is False, proof
        assert proof["observed"]["active"]["profile_uuid"] == ORIGINAL.uuid, proof
        reconcile = {**target(unresolved), "evidence_ref": proof_ref}
        rejects("PermissionDenied", node, "Operation.Reconcile", reconcile, outsider["token"])
        closed = node.call("Operation.Reconcile", reconcile)
        assert (closed["lifecycle"], closed["outcome_knowledge"]) == ("Failed", "Known"), closed
        recovered = node.wait(second)
        assert (recovered["lifecycle"], recovered["outcome_knowledge"]) == ("Failed", "Known"), recovered
        assert artifact(node, recovered)[1] == result
        manager.assert_profile(ORIGINAL)
        node.stop()
        rollback_calls(log, [checkpoint, second_checkpoint])

    with closing(sqlite3.connect(node.state / "state.db")) as db:
        assert db.execute("SELECT count(*) FROM holds WHERE volume=?", (link_id,)).fetchone() == (0,)
    verification = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert verification["database_format"] >= 90 and not verification["migration_required"], verification
    assert verification["sqlite_integrity"] == "Verified", verification
    print("PASS exact operator rollback restores real traffic before timeout and never redispatches a lost receipt")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/debug")
    args = parser.parse_args()
    with isolated_manager() as manager:
        node = Node(args.bin_dir)
        try:
            exercise(node, manager)
        finally:
            node.close()


if __name__ == "__main__":
    main()
