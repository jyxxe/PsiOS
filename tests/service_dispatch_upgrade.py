"""Upgrade an old authority while its real systemd start has an unknown outcome."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import shlex
import signal
import sqlite3
import subprocess

from service_journal import refuses
from services import eventually
from support import Node, key, target


def dispatch_state(db, task, service):
    phase, = db.execute("SELECT phase FROM queue WHERE task=?", (task,)).fetchone()
    hold = db.execute("SELECT holder FROM holds WHERE volume=?", (service,)).fetchone()
    return phase, hold


def assert_legacy_path(db, operation):
    assert db.execute("""SELECT id FROM versions
        WHERE kind IN ('HostServiceControlIntent', 'HostServiceControlCheckpoint')
          AND spec->>'operation_id'=?""", (operation,)).fetchall() == []


def assert_before_control_epoch(db, operation):
    effect, = db.execute("""SELECT seq FROM versions WHERE kind='Effect' AND rev=1
        AND body->>'operation_id'=? AND body->>'action'='service.start'""", (operation,)).fetchone()
    epoch, = db.execute("SELECT seq FROM current WHERE kind='HostServiceControlEpoch'").fetchone()
    assert effect < epoch, (effect, epoch)


def main(previous, previous_format, current):
    previous = previous.resolve(strict=True)
    current = current.resolve(strict=True)
    assert not (previous / "psiosd").samefile(current / "psiosd"), "requires distinct authority binaries"
    node = Node(previous)
    unit = f"psios-dispatch-upgrade-{key()}.service"
    service_id = f"service-{unit}"
    units = Path(f"/run/user/{os.geteuid()}/systemd/user")
    definition = units / unit
    starter = node.base / "start.sh"
    invocations = node.base / "invocations"
    release = node.base / "release"
    ctl = ["systemctl", "--user"]
    database = (node.state / "state.db").as_uri() + "?mode=ro"

    def launches():
        return invocations.read_text().splitlines() if invocations.exists() else []

    try:
        units.mkdir(mode=0o700, parents=True, exist_ok=True)
        starter.write_text(f"""set -eu
printf '%s\\n' "$INVOCATION_ID" >> {shlex.quote(str(invocations))}
while test ! -f {shlex.quote(str(release))}; do sleep .05; done
""")
        definition.write_text(f"""[Unit]
Description=PsiOS legacy start cutover fixture
[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=90
ExecStart=/bin/sh {json.dumps(str(starter))}
""")
        subprocess.run(ctl + ["daemon-reload"], check=True, timeout=10)
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [dict(unit=unit, manager="user", allow_control=False)]
        node.config.write_text(json.dumps(config))
        node.start()
        first = node.perform("service.inspect", target(node.get(service_id)["reference"]))
        assert first["active_state"] == "inactive", first
        node.stop()
        config["services"][0].update(allow_control=True, definition_digest=first["definition_digest"])
        node.config.write_text(json.dumps(config))
        node.start()
        service = node.get(service_id)["reference"]

        request = node.prepare("service.start", target(service))
        task = node.call("Task.Submit", request)["task_id"]
        node.approve(task)
        original, = eventually(lambda: [item for item in launches() if item])
        active = node.call("Task.Get", {"id": task})
        assert (active["lifecycle"], active["outcome_knowledge"]) == ("Running", "InFlight"), active
        run_id = active["current_run_id"]
        operation_id = active["operation_refs"][0]["resource_id"]
        node.stop(signal.SIGKILL)
        with closing(sqlite3.connect(database, uri=True, timeout=10)) as db:
            assert db.execute("PRAGMA user_version").fetchone() == (previous_format,)
            assert dispatch_state(db, task, service_id) == ("Dispatched", (operation_id,))
            assert_legacy_path(db, operation_id)

        node.daemon[0] = str(current / "psiosd")
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        assert before["database_format"] == previous_format and before["migration_required"], before
        node.start()
        paused = node.wait(task)
        assert (paused["lifecycle"], paused["outcome_knowledge"]) == ("Paused", "Unknown"), paused
        assert paused["current_run_id"] == run_id and paused["conditions"][0]["type"] == "OutcomeUnknown", paused
        assert paused["operation_refs"][0]["resource_id"] == operation_id, paused
        assert node.get(service_id)["reference"] == service
        with closing(sqlite3.connect(database, uri=True, timeout=10)) as db:
            assert db.execute("PRAGMA user_version").fetchone()[0] >= 69
            assert dispatch_state(db, task, service_id) == ("Unknown", (operation_id,))
            assert_legacy_path(db, operation_id)
            assert_before_control_epoch(db, operation_id)

        same = node.call("Task.Submit", request)
        assert same["task_id"] == task and same["deduplicated"], same
        refuses("OutcomeUnknown", lambda: node.call("Task.Retry", {
            "id": task, "expected_resource_version": paused["task_revision"], "key": key()}))
        competing = node.wait(node.submit("service.stop", target(service)), autoapprove=True)
        assert (competing["lifecycle"], competing["outcome_knowledge"], competing["conditions"][0]["type"]) == (
            "Failed", "NotStarted", "Conflict"), competing

        release.touch()
        eventually(lambda: subprocess.run(ctl + ["is-active", "--quiet", unit], timeout=5).returncode == 0)
        inspected = node.wait(node.submit("service.inspect", target(service)))
        assert inspected["lifecycle"] == "Succeeded", inspected
        evidence, = inspected["result"]["artifact_refs"]
        live = node.call("Artifact.Read", {"reference": evidence})
        assert live["invocation_id"] == original and live["active_state"] == "active", live
        operation = node.get(operation_id)["reference"]
        settled = node.call("Operation.Reconcile", dict(target(operation), evidence_ref=evidence))
        assert settled["outcome_knowledge"] == "Known", settled
        completed = node.wait(task)
        assert completed["lifecycle"] == "Succeeded" and completed["current_run_id"] == run_id, completed
        result_ref, = completed["result"]["artifact_refs"]
        receipt = node.call("Artifact.Read", {"reference": result_ref})
        assert receipt["observation"]["invocation_id"] == original and "checkpoint_ref" not in receipt, receipt
        final = node.get(operation_id)["value"]
        assert len(final["effect_refs"]) == 1 and launches() == [original], (final, launches())
        node.stop()
        with closing(sqlite3.connect(database, uri=True, timeout=10)) as db:
            assert dispatch_state(db, task, service_id) == ("Done", None)
            assert_legacy_path(db, operation_id)
        subprocess.run(node.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)
        print(f"PASS format {previous_format} live systemd invocation carried through migration and reconciled once")
    finally:
        try:
            release.touch(exist_ok=True)
            subprocess.run(ctl + ["stop", unit], capture_output=True, timeout=20)
        finally:
            try:
                definition.unlink(missing_ok=True)
                subprocess.run(ctl + ["daemon-reload"], capture_output=True, timeout=10)
            finally:
                node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--previous-format", default=34, type=int)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    args = parser.parse_args()
    main(args.previous_bin, args.previous_format, args.current_bin)
