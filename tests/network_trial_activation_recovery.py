"""Recover a lost NetworkManager activation receipt in a private test namespace.

Run with sudo unshare --net --mount --uts --propagation private \
    python3 -B tests/network_trial_activation_recovery.py --bin-dir target/debug
"""

import argparse
from contextlib import closing
import json
import signal
import sqlite3
import subprocess

from network_manager_trial import CANDIDATE, ORIGINAL, boottime, isolated_manager, until
from network_trial import CONFIRMATION_SECONDS, INSPECT, TRIAL, approved_trial, artifact, check_contender, rejects, replay, scope
from network_trial_abort import assert_work_blocker, method_calls, monitor
from support import Node, target


def start_with_publication_fault(node, manager):
    node.stop()
    config = json.loads(node.config.read_text())
    config.update(workers=1, network_link={
        "interface": manager.interface, "mac": manager.mac,
        "original_uuid": ORIGINAL.uuid, "candidate_uuid": CANDIDATE.uuid,
        "confirmation_seconds": CONFIRMATION_SECONDS,
    })
    node.config.chmod(0o600)
    node.config.write_text(json.dumps(config))
    with closing(sqlite3.connect(node.state / "state.db")) as db, db:
        db.execute("""CREATE TRIGGER fail_network_activation_receipt BEFORE INSERT ON versions
            WHEN NEW.kind='NetworkTrialJournal' AND NEW.body->>'phase'='Activated'
            BEGIN SELECT RAISE(ABORT, 'fixture lost the acknowledged activation receipt'); END""")
    node.start()


def lost_activation(node, manager, link, delegation):
    request, task_id = approved_trial(node, link, delegation)
    task = node.wait(task_id)
    assert (task["lifecycle"], task["outcome_knowledge"]) == ("Paused", "Unknown"), task
    assert not task["result"]["artifact_refs"], task
    operation, = task["operation_refs"]
    checkpoint = manager.only_checkpoint(CONFIRMATION_SECONDS)
    manager.assert_profile(CANDIDATE)
    with closing(sqlite3.connect(node.state / "state.db")) as db:
        journal = db.execute("""SELECT body->>'phase', body->'checkpoint'->>'path', body->>'activation'
            FROM current WHERE kind='NetworkTrialJournal' AND spec->>'operation_id'=?""",
            (operation["resource_id"],)).fetchone()
    assert journal == ("ActivateIssued", checkpoint.path, None), journal
    assert_work_blocker(node, task_id, operation, link, "Unknown", "Unknown")
    return request, task_id, operation, checkpoint


def inspect(node, operation, delegation):
    request = node.prepare(INSPECT, target(operation), delegation=delegation)
    return node.wait(node.call("Task.Submit", request)["task_id"])


def assert_inconclusive(node, operation, delegation):
    inspection = inspect(node, operation, delegation)
    assert inspection["lifecycle"] == "Failed" and not inspection["result"]["artifact_refs"], inspection
    assert inspection["conditions"][0]["type"] == "PreconditionFailed", inspection
    assert node.get(operation["resource_id"])["reference"] == operation


def assert_calls(log, activations, destroys):
    for member, count in (("CheckpointCreate", activations), ("ActivateConnection", activations),
                          ("CheckpointDestroy", destroys), ("CheckpointRollback", 0),
                          ("CheckpointAdjustRollbackTimeout", 0)):
        calls = method_calls(log, member)
        assert len(calls) == count, (member, calls)


def exercise(node, manager):
    start_with_publication_fault(node, manager)
    link_id = "network-link-" + manager.mac.replace(":", "")
    link = node.call("NetworkLink.Get", {"id": link_id})["reference"]
    delegation = scope(node, TRIAL, link_id)
    outsider, _ = node.agent(["task.read"])
    manager.assert_profile(ORIGINAL)
    with monitor(manager) as log:
        request, task_id, operation, checkpoint = lost_activation(node, manager, link, delegation)
        assert_calls(log, 1, 0)
        inspect_delegation = scope(node, INSPECT, operation["resource_id"])
        assert_inconclusive(node, operation, inspect_delegation)
        assert manager.checkpoints() == [checkpoint.path] and boottime() < checkpoint.deadline
        rejects("PreconditionFailed", node, "NetworkTrial.Confirmation", target(operation))

        node.stop(signal.SIGKILL)
        node.start()
        assert_work_blocker(node, task_id, operation, link, "Unknown", "Unknown")
        replay(node, request, task_id)
        check_contender(node, link, delegation)
        until("the exact checkpoint expires without an operator call",
              lambda: boottime() >= checkpoint.deadline and manager.checkpoints() == [],
              True, timeout=CONFIRMATION_SECONDS + 6)
        manager.assert_profile(ORIGINAL)
        inspection = inspect(node, operation, inspect_delegation)
        assert (inspection["lifecycle"], inspection["outcome_knowledge"]) == ("Succeeded", "Known"), inspection
        proof_ref, proof = artifact(node, inspection)
        assert (proof["operation_ref"], proof["link_ref"], proof["publication"]) == (
            operation, link, "RolledBack",
        ), proof
        assert proof["observed"]["checkpoint_present"] is False, proof
        assert proof["observed"]["active"]["profile_uuid"] == ORIGINAL.uuid, proof
        reconcile = {**target(operation), "evidence_ref": proof_ref}
        rejects("PermissionDenied", node, "Operation.Reconcile", reconcile, outsider["token"])
        settled = node.call("Operation.Reconcile", reconcile)
        assert (settled["lifecycle"], settled["outcome_knowledge"]) == ("Failed", "Known"), settled
        recovered = node.wait(task_id)
        assert recovered["operation_refs"][0]["resource_id"] == operation["resource_id"], recovered
        assert (recovered["lifecycle"], recovered["outcome_knowledge"]) == ("Failed", "Known"), recovered
        assert artifact(node, recovered)[1] == {"publication": "RolledBack", "link_ref": link}
        assert_calls(log, 1, 0)

        # A new approved trial proves the first hold was released. An external
        # destroy of its checkpoint does not give the unacknowledged trial a commit.
        _, next_task, next_operation, next_checkpoint = lost_activation(node, manager, link, delegation)
        assert next_checkpoint.path != checkpoint.path
        manager.confirm_checkpoint(next_checkpoint)
        assert manager.checkpoints() == []
        manager.assert_profile(CANDIDATE)
        until("the second checkpoint's original deadline elapses",
              lambda: boottime() >= next_checkpoint.deadline, True, timeout=CONFIRMATION_SECONDS + 6)
        assert_inconclusive(node, next_operation, scope(node, INSPECT, next_operation["resource_id"]))
        assert_work_blocker(node, next_task, next_operation, link, "Unknown", "Unknown")
        assert_calls(log, 2, 1)

    node.stop()
    with closing(sqlite3.connect(node.state / "state.db")) as db, db:
        db.execute("DROP TRIGGER fail_network_activation_receipt")
    verification = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert verification["sqlite_integrity"] == "Verified" and not verification["migration_required"], verification
    print("PASS lost activation settles only a proven rollback and preserves an uncertain candidate")


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
