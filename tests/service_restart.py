"""Restart a real test-owned service and reconcile a lost systemd receipt."""
import argparse
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import uuid

from service_journal import refuses
from services import eventually
from support import Node, key, target


def main(previous):
    current = Path("target/debug").absolute()
    node = Node(previous or current)
    unit = f"psios-restart-{key()}.service"
    service_id = f"service-{unit}"
    units = Path(f"/run/user/{os.geteuid()}/systemd/user")
    units.mkdir(mode=0o700, parents=True, exist_ok=True)
    definition = units / unit
    starter = node.base / "start.sh"
    invocations = node.base / "invocations"
    blocking = node.base / "block"
    release = node.base / "release"
    ctl = ["systemctl", "--user"]

    def observed(ref):
        task = node.wait(node.submit("service.inspect", target(ref)))
        assert task["lifecycle"] == "Succeeded", task
        artifact, = task["result"]["artifact_refs"]
        return (node.call("Artifact.Read", {"reference": artifact}), artifact)

    def launches():
        return invocations.read_text().splitlines()

    def verify_failure(message):
        failed = subprocess.run(node.daemon + ["verify"], capture_output=True, timeout=15)
        assert failed.returncode and message in failed.stderr, failed

    try:
        starter.write_text(f"""set -eu
printf '%s\\n' "$INVOCATION_ID" >> {invocations}
if test -f {blocking}; then
    while test ! -f {release}; do sleep 0.05; done
fi
""")
        definition.write_text(f"""[Unit]
Description=PsiOS test-owned restart service
[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=30
ExecStart=/bin/sh {starter}
""")
        subprocess.run(ctl + ["daemon-reload"], check=True, timeout=10)
        subprocess.run(ctl + ["start", unit], check=True, timeout=15)
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [{"unit": unit, "manager": "user", "allow_control": False}]
        node.config.write_text(json.dumps(config))
        node.start()
        first, _ = observed(node.get(service_id)["reference"])
        assert launches() == [first["invocation_id"]]
        node.stop()
        config["services"][0].update(allow_control=True, definition_digest=first["definition_digest"])
        node.config.write_text(json.dumps(config))
        node.start()
        service = node.get(service_id)["reference"]
        scoped = dict(principal_id="local-owner", units=12, ttl_seconds=120, tool_targets=[service_id])
        old = node.call("Grant.Issue", dict(scoped, actions=["task.submit", "task.read", "service.inspect", "service.start"]))
        if previous:
            node.stop(signal.SIGKILL)
            node.daemon[0] = str(current / "psiosd")
            before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
            assert before["database_format"] == 34 and before["migration_required"], before
            node.start()
            assert node.get(service_id)["reference"] == service

        first, _ = observed(service)
        assert (first["manager"], first["manager_uid"]) == ("user", os.geteuid()), first
        request = dict(target(service), expected_boot_id=first["boot_id"], expected_invocation_id=first["invocation_id"])
        refuses("PermissionDenied", lambda: node.prepare("service.restart", request))
        refuses("PermissionDenied", lambda: node.prepare("service.restart", request,
                delegation=old["delegation_ref"]["resource_id"]))
        actions = ["task.submit", "task.read", "service.restart"]
        refuses("InvalidArgument", lambda: node.call("Grant.Issue", dict(scoped, actions=actions, tool_targets=None)))
        approved = node.call("Grant.Issue", dict(scoped, actions=actions))
        delegation = approved["delegation_ref"]["resource_id"]

        def prepare(args):
            return node.prepare("service.restart", args, delegation=delegation)

        def submit(args):
            request = prepare(args)
            return request, node.call("Task.Submit", request)["task_id"]

        refuses("InvalidArgument", lambda: prepare(dict(request, expected_invocation_id="incorrect")))
        _, stale = submit(dict(request, expected_boot_id=str(uuid.uuid4())))
        assert node.wait(stale)["lifecycle"] == "WaitingApproval"
        node.approve(stale)
        rejected = node.wait(stale)
        assert rejected["lifecycle"] == "Failed" and rejected["conditions"][0]["type"] == "PreconditionFailed", rejected
        assert launches() == [first["invocation_id"]]

        database = node.state / "state.db"
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("""CREATE TRIGGER refuse_systemd_checkpoint BEFORE INSERT ON versions
                WHEN NEW.kind='HostServiceControlCheckpoint'
                BEGIN SELECT RAISE(ABORT,'injected checkpoint failure'); END""")
        try:
            _, refused = submit(request)
            blocked = node.wait(refused, autoapprove=True)
            assert (blocked["lifecycle"], blocked["outcome_knowledge"], blocked["conditions"][0]["type"]) == (
                "Failed", "NotStarted", "StorageUnavailable"), blocked
            assert launches() == [first["invocation_id"]], "systemctl ran after checkpoint refused"
        finally:
            with sqlite3.connect(database, timeout=10) as db:
                db.execute("DROP TRIGGER refuse_systemd_checkpoint")

        original, restart = submit(request)
        assert node.wait(restart)["lifecycle"] == "WaitingApproval"
        assert launches() == [first["invocation_id"]]
        node.approve(restart)
        completed = node.wait(restart)
        assert completed["lifecycle"] == "Succeeded", completed
        response = node.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
        checkpoint = node.call("Resource.Resolve", {"reference": response["checkpoint_ref"]})["value"]
        assert checkpoint["verb"] == "try-restart" and checkpoint["before"]["invocation_id"] == first["invocation_id"], checkpoint
        assert checkpoint["before"]["manager_uid"] == os.geteuid() and checkpoint["before"]["boot_id"] == first["boot_id"], checkpoint
        second = response["observation"]["invocation_id"]
        assert second != first["invocation_id"] and launches() == [first["invocation_id"], second]
        duplicate = node.call("Task.Submit", original)
        assert duplicate["deduplicated"] and duplicate["task_id"] == restart

        service = response["service_ref"]
        prior, old_artifact = observed(service)
        blocking.touch()
        _, crashing = submit(dict(target(service), expected_boot_id=prior["boot_id"], expected_invocation_id=second))
        node.approve(crashing)
        eventually(lambda: len(launches()) == 3)
        third = launches()[2]
        assert third not in [first["invocation_id"], second]
        with sqlite3.connect(database, timeout=10) as db:
            checkpoint_id, = db.execute("SELECT id FROM current WHERE kind='HostServiceControlCheckpoint' "
                                        "ORDER BY seq DESC LIMIT 1").fetchone()
        node.stop(signal.SIGKILL)
        node.start()
        unknown = node.wait(crashing)
        assert (unknown["lifecycle"], unknown["outcome_knowledge"]) == ("Paused", "Unknown"), unknown
        operation = unknown["operation_refs"][0]
        reconcile = target(operation)
        node.stop()
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("DELETE FROM holds WHERE holder=?", (operation["resource_id"],))
        verify_failure(b"expected exclusive service hold")
        startup = subprocess.run(node.daemon + ["serve"], capture_output=True, timeout=15)
        assert startup.returncode and b"RecoveryRequired" in startup.stderr, startup
        assert b"psiosd ready" not in startup.stderr, startup
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("INSERT INTO holds(volume,holder) VALUES(?,?)", (service_id, operation["resource_id"]))
        subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)
        node.start()
        competing = node.submit("service.stop", target(service))
        node.approve(competing)
        fenced = node.wait(competing)
        assert fenced["lifecycle"] == "Failed" and fenced["conditions"][0]["type"] == "Conflict", fenced
        refuses("OutcomeUnknown", lambda: node.call("Operation.Reconcile", dict(reconcile, evidence_ref=old_artifact)))
        starting, partial = observed(service)
        assert starting["active_state"] == "activating", starting
        refuses("OutcomeUnknown", lambda: node.call("Operation.Reconcile", dict(reconcile, evidence_ref=partial)))

        release.touch()
        eventually(lambda: subprocess.run(ctl + ["is-active", "--quiet", unit]).returncode == 0)
        running, proof = observed(service)
        assert running["invocation_id"] == third, running
        result = node.call("Operation.Reconcile", dict(reconcile, evidence_ref=proof))
        recovered = node.wait(crashing)
        assert result["outcome_knowledge"] == "Known" and recovered["lifecycle"] == "Succeeded", result
        recovered_result = node.call("Artifact.Read", {"reference": recovered["result"]["artifact_refs"][0]})
        assert recovered_result["checkpoint_ref"]["resource_id"] == checkpoint_id, recovered_result
        assert launches() == [first["invocation_id"], second, third]
        witness_task = node.call("Artifact.Get", {"reference": proof})["resource"]["spec"]["task_id"]
        node.call("Task.EvictResults", dict(target(node.get(witness_task)["reference"]), reason="inspected effect is already settled"))
        refuses("ResultEvicted", lambda: node.call("Artifact.Read", {"reference": proof}))
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 69 and not after["migration_required"], after
        with sqlite3.connect(database, timeout=10) as db:
            record, = db.execute("SELECT body FROM versions WHERE id=? AND rev=1", (checkpoint_id,)).fetchone()
            db.execute("UPDATE versions SET body=json_set(body,'$.before.manager_uid',2999999999) WHERE id=? AND rev=1",
                       (checkpoint_id,))
        verify_failure(b"missing or unbound checkpoint")
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (record, checkpoint_id))
            operation_id = operation["resource_id"]
            db.execute("INSERT INTO holds(volume,holder) VALUES(?,?)", (service_id, operation_id))
        verify_failure(b"expected exclusive service hold")
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("DELETE FROM holds WHERE holder=?", (operation_id,))
            approval_id, = db.execute("SELECT decision FROM approval_bindings WHERE operation=?", (operation_id,)).fetchone()
            approval, = db.execute("SELECT body FROM versions WHERE id=? AND rev=1", (approval_id,)).fetchone()
            db.execute("UPDATE versions SET body=json_set(body,'$.decision','Rejected') WHERE id=? AND rev=1", (approval_id,))
        verify_failure(b"reference digest mismatch")
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (approval, approval_id))
            confirmation_id, confirmation = db.execute(
                "SELECT id,body FROM current WHERE kind='HostServiceControlConfirmation' AND spec->>'operation_id'=?",
                (operation_id,)).fetchone()
            db.execute("UPDATE versions SET body=json_set(body,'$.result_ref',json(?)) WHERE id=? AND rev=1",
                       (json.dumps(completed["result"]["artifact_refs"][0]), confirmation_id))
        verify_failure(b"settled receipt disagrees")
        with sqlite3.connect(database, timeout=10) as db:
            db.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (confirmation, confirmation_id))
        subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)
        print("PASS real restart, approval, boot match, durable deduplication and lost-receipt reconciliation")
    finally:
        release.touch(exist_ok=True)
        subprocess.run(ctl + ["stop", unit], check=False, timeout=20)
        definition.unlink(missing_ok=True)
        subprocess.run(ctl + ["daemon-reload"], check=False, timeout=10)
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin")
    main(parser.parse_args().previous_bin)
