"""Exercise PsiOS network trials through its Unix API and CLI.

Run with sudo unshare --net --mount --uts --propagation private \
    python3 -B tests/network_trial.py --bin-dir target/debug
"""
import argparse
import json
import signal
import subprocess
import time

from network_manager_trial import CANDIDATE, ORIGINAL, boottime, isolated_manager, until
from support import Failure, Node, key, target


TRIAL = "network.trial"
INSPECT = "network.inspect_trial"
CONFIRMATION_SECONDS = 24


def rejects(code, node, method, params, token=None):
    try:
        node.call(method, params, token)
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"{method} unexpectedly succeeded")


def scope(node, tool, resource_id):
    grant = node.call("Grant.Issue", {
        "principal_id": "local-owner",
        "actions": [tool, "task.submit", "task.read"],
        "tool_targets": [resource_id],
        "units": 16,
        "ttl_seconds": 300,
    })
    return grant["delegation_ref"]["resource_id"]


def approved_trial(node, link, delegation):
    request = node.prepare(TRIAL, target(link), delegation=delegation)
    task_id = node.call("Task.Submit", request)["task_id"]
    node.approve(task_id)
    return request, task_id


def waiting(node, task_id):
    deadline = time.monotonic() + 14
    while True:
        task = node.call("Task.Get", {"id": task_id})
        if task["lifecycle"] == "WaitingInput":
            break
        assert task["lifecycle"] in ("Queued", "Running") and time.monotonic() < deadline, task
        time.sleep(0.05)

    assert task["outcome_knowledge"] == "InFlight", task
    assert task["conditions"][0]["type"] == "AwaitingConfirmation", task
    operation, = task["operation_refs"]
    run = node.get(task["current_run_id"])["value"]
    assert (run["lifecycle"], run["outcome_knowledge"], run["operation_refs"]) == (
        "WaitingInput", "InFlight", [operation],
    ), run
    current = node.get(operation["resource_id"])
    assert current["reference"] == operation, current
    assert (current["value"]["lifecycle"], current["value"]["outcome_knowledge"]) == (
        "WaitingDependency", "InFlight",
    ), current
    return operation


def check_contender(node, link, delegation):
    _, contender = approved_trial(node, link, delegation)
    blocked = node.wait(contender)
    assert (blocked["lifecycle"], blocked["outcome_knowledge"], blocked["conditions"][0]["type"]) == (
        "Failed", "NotStarted", "Conflict",
    ), blocked


def artifact(node, task):
    reference, = task["result"]["artifact_refs"]
    return reference, node.call("Artifact.Read", {"reference": reference})


def replay(node, request, task_id):
    same = node.call("Task.Submit", request)
    assert same["deduplicated"] and same["task_id"] == task_id, same


def exercise(node, manager):
    node.stop()
    config = json.loads(node.config.read_text())
    enrollment = {
        "interface": manager.interface,
        "mac": manager.mac,
        "original_uuid": ORIGINAL.uuid,
        "candidate_uuid": CANDIDATE.uuid,
        "confirmation_seconds": CONFIRMATION_SECONDS,
    }
    config.update(workers=1, network_link=enrollment)
    node.config.chmod(0o600)
    node.config.write_text(json.dumps(config))
    node.start()

    owner_token = node.witness / "owner.token"
    link_id = "network-link-" + manager.mac.replace(":", "")
    outsider, _ = node.agent(["task.read"])
    rejects("PermissionDenied", node, "NetworkLink.Get", {"id": link_id}, outsider["token"])
    rejects("PermissionDenied", node, "NetworkLink.Get", {"id": link_id})
    delegation = scope(node, TRIAL, link_id)
    enrolled = node.call("NetworkLink.Get", {"id": link_id})
    assert enrolled["configured"] and enrolled["state"]["lifecycle"] == "Registered", enrolled
    assert enrolled["configuration"] == enrollment, enrolled
    assert node.cli(owner_token, "network-link", link_id) == enrolled
    link = enrolled["reference"]
    assert node.get(link_id)["reference"] == link

    rejects("PermissionDenied", node, "Task.Prepare", {
        "tool": TRIAL, "input": target(link), "key": key(), "delegation_id": "owner-delegation",
    })

    # A separate local controller already owns a checkpoint for this exact link.
    foreign_seconds = 90
    foreign = manager.checkpoint(foreign_seconds)
    try:
        check_contender(node, link, delegation)
        assert manager.only_checkpoint(foreign_seconds) == foreign
        assert boottime() < foreign.deadline
        manager.assert_profile(ORIGINAL)
    finally:
        if manager.checkpoints() == [foreign.path]:
            manager.confirm_checkpoint(foreign)
    assert manager.checkpoints() == []

    request = node.prepare(TRIAL, target(link), delegation=delegation)
    task_id = node.call("Task.Submit", request)["task_id"]
    pending = node.call("Task.Get", {"id": task_id})
    assert (pending["lifecycle"], pending["outcome_knowledge"]) == ("WaitingApproval", "NotStarted"), pending
    assert manager.checkpoints() == []
    manager.assert_profile(ORIGINAL)
    node.approve(task_id)

    old_operation = waiting(node, task_id)
    manager.assert_profile(CANDIDATE)
    old_checkpoint = manager.only_checkpoint(CONFIRMATION_SECONDS)
    rejects("PermissionDenied", node, "NetworkTrial.Confirmation", target(old_operation), outsider["token"])
    old_challenge = node.call("NetworkTrial.Confirmation", target(old_operation))
    assert old_challenge["operation_ref"] == old_operation
    old_token = old_challenge["confirmation_token"]
    assert old_token and old_challenge["expires_at"]

    # The only worker can execute independent work while this trial parks with the link hold.
    assert node.perform("node.inventory", {})["boot_id"]
    replay(node, request, task_id)
    check_contender(node, link, delegation)
    assert manager.checkpoints() == [old_checkpoint.path]
    assert boottime() < old_checkpoint.deadline

    # No Core process is alive to produce the rollback. NetworkManager owns the timer.
    node.stop(signal.SIGKILL)
    assert node.process.poll() == -signal.SIGKILL
    until("unconfirmed checkpoint expires with Core stopped", manager.checkpoints, [],
          timeout=CONFIRMATION_SECONDS + 6)
    manager.assert_profile(ORIGINAL)
    node.start()

    paused = node.wait(task_id)
    assert (paused["lifecycle"], paused["outcome_knowledge"], paused["conditions"][0]["type"]) == (
        "Paused", "Unknown", "OutcomeUnknown",
    ), paused
    operation, = paused["operation_refs"]
    assert operation["resource_id"] == old_operation["resource_id"] and operation != old_operation
    assert not paused["result"]["artifact_refs"]
    replay(node, request, task_id)
    rejects("PreconditionFailed", node, "NetworkTrial.Confirm", {
        **target(operation), "confirmation_token": old_token,
    })
    check_contender(node, link, delegation)
    assert manager.checkpoints() == []

    rejects("PermissionDenied", node, "Task.Prepare", {
        "tool": INSPECT, "input": target(operation), "key": key(), "delegation_id": "owner-delegation",
    })
    inspection_scope = scope(node, INSPECT, operation["resource_id"])
    inspection = node.prepare(INSPECT, target(operation), delegation=inspection_scope)
    checked = node.wait(node.call("Task.Submit", inspection)["task_id"])
    assert (checked["lifecycle"], checked["outcome_knowledge"]) == ("Succeeded", "Known"), checked
    evidence, proof = artifact(node, checked)
    assert (proof["operation_ref"], proof["link_ref"], proof["publication"]) == (
        operation, link, "RolledBack",
    ), proof
    assert not proof["observed"]["checkpoint_present"], proof
    assert proof["observed"]["active"]["profile_uuid"] == ORIGINAL.uuid, proof

    reconciliation = {**target(operation), "evidence_ref": evidence}
    rejects("PermissionDenied", node, "Operation.Reconcile", reconciliation, outsider["token"])
    result = node.call("Operation.Reconcile", reconciliation)
    assert (result["lifecycle"], result["outcome_knowledge"]) == ("Failed", "Known"), result
    rolled_back = node.wait(task_id)
    assert (rolled_back["lifecycle"], rolled_back["outcome_knowledge"]) == ("Failed", "Known"), rolled_back
    assert artifact(node, rolled_back)[1] == {"publication": "RolledBack", "link_ref": link}
    manager.assert_profile(ORIGINAL)

    second_request, second = approved_trial(node, link, delegation)
    current = waiting(node, second)
    assert second != task_id and current["resource_id"] != operation["resource_id"], current
    checkpoint = manager.only_checkpoint(CONFIRMATION_SECONDS)
    assert checkpoint.path != old_checkpoint.path, checkpoint
    manager.assert_profile(CANDIDATE)
    rejects("PermissionDenied", node, "NetworkTrial.Confirm", {
        **target(current), "confirmation_token": old_token,
    })
    assert manager.checkpoints() == [checkpoint.path]
    challenge = node.cli(owner_token, "network-trial-confirmation", current["resource_id"],
                         "--revision", current["version"])
    assert challenge["operation_ref"] == current
    assert challenge["confirmation_token"] != old_token
    path = node.base / "network-confirmation.json"
    path.touch(mode=0o600)
    path.write_text(json.dumps(challenge))
    try:
        confirmed = node.cli(owner_token, "network-trial-confirm", current["resource_id"],
                             "--revision", current["version"], path)
    finally:
        path.unlink()
    assert (confirmed["lifecycle"], confirmed["outcome_knowledge"]) == ("Succeeded", "Known"), confirmed
    committed = node.wait(second)
    assert (committed["lifecycle"], committed["outcome_knowledge"]) == ("Succeeded", "Known"), committed
    assert artifact(node, committed)[1] == {"publication": "Committed", "link_ref": link}
    replay(node, second_request, second)
    assert manager.checkpoints() == []
    while boottime() <= checkpoint.deadline + 0.25:
        time.sleep(0.15)
    manager.assert_profile(CANDIDATE)

    node.stop()
    verification = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert verification["database_format"] >= 78 and not verification["migration_required"], verification
    print("PASS approved network trial rolls back without Core and a new CLI-confirmed trial survives its deadline")


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
