"""Fence a real systemd observation after its authority or Task is withdrawn."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import time

from services import eventually
from support import Node, key, target
from test_schedule import timestamp


def observe_in_flight(node, task, unit, trace):
    """Wait for this daemon's actual unit query inside a traced exec boundary."""
    workers = Path(f"/proc/{node.process.pid}/task")

    def unit_query():
        for thread in workers.iterdir():
            try:
                children = (thread / "children").read_text().split()
            except FileNotFoundError:
                continue
            for child in children:
                try:
                    argv = Path(f"/proc/{child}/cmdline").read_bytes().split(b"\0")
                except (FileNotFoundError, ProcessLookupError):
                    continue
                if argv[:2] == [b"/usr/bin/systemctl", b"--user"] and argv[-4:] == [
                    b"--property=Id,LoadState,ActiveState,SubState,MainPID,InvocationID,NeedDaemonReload",
                    b"--", unit.encode(), b"",
                ]:
                    return child
        return None

    try:
        eventually(unit_query, timeout=12)
    except AssertionError as error:
        raise AssertionError(trace.read_text()) from error
    running = node.get(task)
    assert running["value"]["lifecycle"] == "Running", running
    return running["reference"]


def check_case(bin_dir, tracer, case):
    node = Node(bin_dir)
    unit = f"psios-observe-authority-{key()}.service"
    service = f"service-{unit}"
    state_id = f"state-{service}"
    units = Path(f"/run/user/{os.geteuid()}/systemd/user")
    units.mkdir(mode=0o700, parents=True, exist_ok=True)
    definition = units / unit
    trace = node.base / "service-observation.strace"

    def control(*args, check=True):
        return subprocess.run(["/usr/bin/systemctl", "--user", *args], check=check, timeout=15)

    def grant():
        return node.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 10, "ttl_seconds": 120,
            "tool_targets": [service], "actions": ["task.submit", "task.read", "service.observe"],
        })

    def submit(as_grant):
        delegation = as_grant["delegation_ref"]["resource_id"]
        request = node.prepare("service.observe", parameters, delegation=delegation)
        return node.call("Task.Submit", request)["task_id"]

    database = (node.state / "state.db").as_uri() + "?mode=ro"

    def position():
        with closing(sqlite3.connect(database, uri=True)) as db:
            return db.execute(
                "SELECT dispatched_seq FROM service_observation_positions WHERE service=?",
                [service],
            ).fetchone()[0]

    def watch_from_now():
        return node.call("Resource.List", {"kind": "HostServiceState", "id": state_id})["watch_start_cursor"]

    def triggers():
        return node.call("Schedule.Get", {"id": schedule["resource_id"]})["state"]["trigger_refs"]

    try:
        definition.write_text("[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n")
        control("daemon-reload")
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [{"unit": unit, "manager": "user"}]
        node.config.write_text(json.dumps(config))
        node.start()
        metadata = node.get(service)["resource"]["metadata"]
        parameters = {"id": service, "expected_spec_generation": metadata["spec_generation"]}
        original, replacement = grant(), grant()
        previous = node.wait(submit(original))
        assert previous["lifecycle"] == "Succeeded", previous
        earlier, = previous["result"]["artifact_refs"]
        previous_sample = node.call("Artifact.Read", {"reference": earlier})
        assert previous_sample["observation"]["active_state"] == "inactive", previous_sample
        state = previous_sample["service_state_ref"]
        saved_position = position()
        schedule_grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 1, "ttl_seconds": 120,
            "tool_targets": ["profile-local", state_id],
            "actions": ["schedule.manage", "task.submit", "task.read", "resource.read", "event.read", "system.status"],
        })
        schedule = node.call("Schedule.Create", {
            "key": key(), "tool": "system.status", "input": {},
            "delegation_id": schedule_grant["delegation_ref"]["resource_id"],
            "event": {"source_ref": state, "condition": {"kind": "LifecycleEntered", "lifecycle": "Active"},
                      "expires_at": timestamp(60), "max_triggers": 1},
        })["reference"]
        watch = watch_from_now()
        control("start", unit)
        control("is-active", "--quiet", unit)

        node.stop()
        normal = node.daemon
        # Delay each actual systemctl exec inside this daemon. At the observed
        # unit query, at least the real cat and final manager sample still have
        # to run before any result can reach the broker.
        node.daemon = [tracer, "-D", "-f", "-o", str(trace), "-e", "trace=execve",
                       "-e", "inject=execve:delay_exit=1800ms", "--", *normal]
        try:
            node.start()
        finally:
            node.daemon = normal
        assert os.path.samefile(f"/proc/{node.process.pid}/exe", normal[0])
        task = submit(original)
        running = observe_in_flight(node, task, unit, trace)
        if case == "revoke":
            revocation = node.call("Grant.Revoke", target(original["grant_ref"]))
            assert revocation["status"] == "Partial", revocation
            # The current authorization epoch deliberately invalidates the
            # prior cursor, so take a new one while the real read is in flight.
            watch = watch_from_now()
            expected = "Failed", "PermissionDenied"
        else:
            cancellation = node.call("Task.Cancel", target(running))
            assert cancellation["lifecycle"] == "Cancelling", cancellation
            expected = "Cancelled", "Fenced"

        settled = node.wait(task)
        assert node.get(state_id)["reference"] == state, "the fenced service read published shared state"
        assert position() == saved_position
        assert not node.call("Resource.Watch", {"cursor": watch})["events"]
        assert not triggers()
        assert settled["outcome_knowledge"] == "Known", settled
        assert (settled["lifecycle"], settled["conditions"][0]["type"]) == expected, settled
        assert settled["result"] == {"availability": "Unavailable", "artifact_refs": []}, settled
        operation = settled["operation_refs"][0]["resource_id"]
        with closing(sqlite3.connect(database, uri=True)) as db:
            bodies = db.execute("""
                SELECT kind FROM versions
                WHERE (kind='Artifact' AND spec->>'task_id'=?)
                   OR (kind='LateReceipt' AND body->'operation_ref'->>'resource_id'=?
                       AND json_type(body,'$.evidence.receipt') IS NOT NULL)
            """, [task, operation]).fetchall()
            assert not bodies, bodies
        assert node.call("Artifact.Read", {"reference": earlier}) == previous_sample

        node.stop()
        node.start()
        assert node.get(state_id)["reference"] == state and position() == saved_position
        assert not triggers()
        fresh = node.wait(submit(replacement))
        assert fresh["lifecycle"] == "Succeeded", fresh
        artifact, = fresh["result"]["artifact_refs"]
        result = node.call("Artifact.Read", {"reference": artifact})
        assert result["projection"] == "Published" and result["observation"]["active_state"] == "active", result
        updated = result["service_state_ref"]
        assert node.get(state_id)["reference"] == updated and position() > saved_position
        event, = node.call("Resource.Watch", {"cursor": watch})["events"]
        trigger_ref, = eventually(triggers)
        trigger = node.call("Schedule.TriggerGet", {"id": trigger_ref["resource_id"]})["state"]
        assert (event["event_id"], event["resource_ref"]) == (trigger["source_event"]["event_id"], updated)
        child = node.wait(trigger["task_receipt"]["task_id"])
        assert child["lifecycle"] == "Succeeded", child
        node.stop()
        verified = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        assert not verified["migration_required"], verified
        print(f"PASS real service observation {case} fence, restart, and independently authorized publication")
    finally:
        try:
            node.close()
        finally:
            control("stop", unit, check=False)
            definition.unlink(missing_ok=True)
            control("daemon-reload", check=False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", default="target/debug")
    parser.add_argument("--case", choices=("cancel", "revoke"), action="append")
    args = parser.parse_args()
    tracer = shutil.which("strace")
    if tracer is None:
        parser.error("strace is required for the systemd observation race")
    for selected in args.case or ("revoke", "cancel"):
        check_case(args.bin_dir, tracer, selected)
