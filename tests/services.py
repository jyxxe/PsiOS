"""Real systemd user manager, worker concurrency and post-crash readback of a service effect."""
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time

from support import Failure, Node, key, target


def eventually(check, timeout=12):
    expiry = time.monotonic() + timeout
    while time.monotonic() < expiry:
        value = check()
        if value:
            return value
        time.sleep(.03)
    raise AssertionError("condition timed out")


def main():
    n = Node("target/debug")
    unit = "psios-accept-" + key() + ".service"
    service_id = "service-" + unit
    units = Path(f"/run/user/{os.geteuid()}/systemd/user")
    units.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = units / unit
    completion = n.base / "allow-start"
    entered = n.base / "starting"
    ctl = ["systemctl", "--user"]

    def checkpoint(receipt, verb, before):
        saved = n.call("Resource.Resolve", {"reference": receipt["checkpoint_ref"]})["value"]
        intent = n.call("Resource.Resolve", {"reference": saved["intent_ref"]})["value"]
        assert saved["verb"] == verb and saved["before"]["active_state"] == before, saved
        assert intent["tool"] == f"service.{verb}", intent
        assert intent["service_ref"]["resource_id"] == service_id, intent
        assert saved["before"]["manager_uid"] == os.geteuid(), saved
        for field in ("unit", "manager", "manager_uid", "boot_id", "definition_digest"):
            assert saved["before"][field] == receipt["observation"][field], (saved, receipt)
        return intent

    try:
        path.write_text(f"""[Unit]
Description=PsiOS test-owned barrier
[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=30
ExecStart=/bin/sh -c 'touch {entered}; while test ! -f {completion}; do sleep .05; done'
""")
        subprocess.run(ctl + ["daemon-reload"], check=True)
        n.stop()
        cfg = json.loads(n.config.read_text())
        cfg.update(workers=3, services=[dict(unit=unit, manager="user", allow_control=False)])
        n.config.write_text(json.dumps(cfg))
        n.start()
        service = n.get(service_id)["reference"]
        initial = n.perform("service.inspect", target(service))
        original_unit = path.read_text()
        n.stop()
        cfg["services"][0].update(allow_control=True, definition_digest=initial["definition_digest"])
        n.config.write_text(json.dumps(cfg))
        n.start()
        service = n.get(service_id)["reference"]
        held_plan = n.submit("service.start", target(service))
        path.write_text(original_unit.replace("test-owned barrier", "different definition"))
        subprocess.run(ctl + ["daemon-reload"], check=True)
        n.approve(held_plan)
        fenced = n.wait(held_plan)
        assert fenced["conditions"][0]["type"] == "ApprovalInvalidated" and not entered.exists(), fenced
        path.write_text(original_unit)
        subprocess.run(ctl + ["daemon-reload"], check=True)

        with sqlite3.connect(n.state / "state.db", timeout=10) as db:
            db.execute("""CREATE TRIGGER refuse_start_checkpoint BEFORE INSERT ON versions
                WHEN NEW.kind='HostServiceControlCheckpoint'
                BEGIN SELECT RAISE(ABORT,'injected start checkpoint failure'); END""")
        try:
            denied_id = n.submit("service.start", target(service))
            denied = n.wait(denied_id, autoapprove=True)
            assert (denied["lifecycle"], denied["outcome_knowledge"], denied["conditions"][0]["type"]) == (
                "Failed", "NotStarted", "StorageUnavailable"), denied
            assert not entered.exists(), "systemd ran after the checkpoint was refused"
            assert n.perform("service.inspect", target(service))["active_state"] == "inactive"
        finally:
            with sqlite3.connect(n.state / "state.db", timeout=10) as db:
                db.execute("DROP TRIGGER refuse_start_checkpoint")

        prior = n.wait(n.submit("service.inspect", target(service)))
        assert prior["lifecycle"] == "Succeeded", prior
        old_evidence = prior["result"]["artifact_refs"][0]
        start = n.call("Task.Retry", dict(target(n.get(denied_id)["reference"]), key=key()))["task_id"]
        assert start != denied_id and n.wait(start)["lifecycle"] == "WaitingApproval"
        n.approve(start)
        eventually(entered.exists)
        assert n.call("Task.Get", {"id": start})["outcome_knowledge"] == "InFlight"
        assert n.perform("node.inventory", {})["boot_id"]
        assert not completion.exists()
        n.stop(signal.SIGKILL)
        n.start()
        lost = n.wait(start)
        assert lost["outcome_knowledge"] == "Unknown" and lost["lifecycle"] == "Paused", lost
        op = n.get(lost["operation_refs"][0]["resource_id"])["reference"]
        try:
            n.call("Operation.Reconcile", dict(**target(op), evidence_ref=old_evidence))
        except Failure as e:
            assert e.code == "OutcomeUnknown", e
        else:
            raise AssertionError("old systemd observation accepted")
        completion.touch()
        eventually(lambda: subprocess.run(ctl + ["is-active", "--quiet", unit]).returncode == 0)
        evidence = n.wait(n.submit("service.inspect", target(service)))["result"]["artifact_refs"][0]
        observed = n.call("Artifact.Read", {"reference": evidence})
        assert observed["active_state"] == "active" and observed["invocation_id"], observed
        # Readback, settling the old task and releasing the service hold happen without a second start.
        n.call("Operation.Reconcile", dict(**target(op), evidence_ref=evidence))
        completed = n.wait(start)
        assert completed["lifecycle"] == "Succeeded", completed
        receipt = n.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
        intent = checkpoint(receipt, "start", "inactive")
        assert intent["operation_ref"]["resource_id"] == op["resource_id"], intent
        service = n.get(service_id)["reference"]
        stopped = n.perform("service.stop", target(service))
        assert stopped["observation"]["active_state"] == "inactive"
        checkpoint(stopped, "stop", "active")
        agent, grant = n.agent(["task.submit", "task.read", "service.start"])
        req = n.prepare("service.start", target(stopped["service_ref"]), key(), agent["token"], grant["delegation_ref"]["resource_id"])
        live = n.call("Task.Submit", req, agent["token"])["task_id"]
        n.approve(live)
        done = n.wait(live)
        assert done["lifecycle"] == "Succeeded", done
        n.call("Task.Archive", dict(id=live, expected_resource_version=done["task_revision"]))
        n.stop()
        n.start()
        residual = n.call("Grant.Revoke", target(grant["grant_ref"]))
        assert residual["status"] == "Partial" and residual["targets"][0]["residual_effects"], residual
        n.perform("service.stop", target(n.get(service_id)["reference"]))
        n.stop()
        cfg["services"][0]["allow_control"] = False
        n.config.write_text(json.dumps(cfg))
        n.start()
        service = n.get(service_id)["reference"]
        failed = n.wait(n.submit("service.start", target(service)), autoapprove=True)
        assert failed["conditions"][0]["type"] == "PermissionDenied", failed
        assert n.perform("service.inspect", target(service))["active_state"] == "inactive"
        print("PASS real systemd start/stop checkpoints, denied dispatch, concurrent workers and SIGKILL recovery")
    finally:
        completion.touch()
        subprocess.run(ctl + ["stop", unit], capture_output=True)
        path.unlink(missing_ok=True)
        subprocess.run(ctl + ["daemon-reload"], check=True)
        n.close()


if __name__ == "__main__":
    main()
