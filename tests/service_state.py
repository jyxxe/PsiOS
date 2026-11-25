"""Publish real service state transitions and feed the durable event scheduler."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess

from service_journal import refuses
from services import eventually
from support import Node, key, target
from test_schedule import timestamp


def main(previous):
    current = Path("target/debug").absolute()
    node = Node(previous or current)
    unit = f"psios-state-{key()}.service"
    service_id = f"service-{unit}"
    state_id = f"state-{service_id}"
    units = Path(f"/run/user/{os.geteuid()}/systemd/user")
    units.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = units / unit
    ctl = ["systemctl", "--user"]

    def sqlexec(sql):
        with closing(sqlite3.connect(node.state / "state.db")) as db, db:
            db.execute(sql)

    def position():
        with closing(sqlite3.connect(f"{(node.state / 'state.db').as_uri()}?mode=ro", uri=True)) as db:
            return db.execute("SELECT dispatched_seq FROM service_observation_positions WHERE service=?",
                              (service_id,)).fetchone()[0]

    try:
        path.write_text("[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n")
        subprocess.run(ctl + ["daemon-reload"], check=True, timeout=10)
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [{"unit": unit, "manager": "user"}]
        node.config.write_text(json.dumps(config))
        node.start()
        service = node.get(service_id)
        original = service["reference"]
        old = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 12, "ttl_seconds": 120,
                                       "tool_targets": [service_id], "actions": ["task.submit", "task.read", "service.inspect"]})
        if previous:
            node.stop(signal.SIGKILL)
            node.daemon[0] = str(current / "psiosd")
            before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
            assert before["database_format"] == 35 and before["migration_required"], before
            node.start()
            assert node.get(service_id)["reference"] == original
        initial = node.get(state_id)
        assert initial["value"]["lifecycle"] == "Unknown", initial
        request = {"id": service_id, "expected_spec_generation": service["resource"]["metadata"]["spec_generation"]}
        refuses("PermissionDenied", lambda: node.prepare("service.observe", request))
        refuses("PermissionDenied", lambda: node.prepare("service.observe", request,
                delegation=old["delegation_ref"]["resource_id"]))
        grant_request = {"principal_id": "local-owner", "actions": ["task.submit", "task.read", "service.observe"],
                         "units": 24, "ttl_seconds": 120, "tool_targets": [service_id]}
        refuses("InvalidArgument", lambda: node.call("Grant.Issue", dict(grant_request, tool_targets=None)))
        grant = node.call("Grant.Issue", grant_request)
        delegation = grant["delegation_ref"]["resource_id"]

        def submit(params=request):
            saved = node.prepare("service.observe", params, delegation=delegation)
            return node.call("Task.Submit", saved)["task_id"]

        def observe():
            task = node.wait(submit())
            assert task["lifecycle"] == "Succeeded", task
            return node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})

        refuses("InvalidArgument", lambda: submit(dict(request, expected_spec_generation="00")))
        inactive = observe()
        assert inactive["projection"] == "Published" and inactive["observation"]["active_state"] == "inactive", inactive
        source = inactive["service_state_ref"]
        schedule_grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "actions": ["schedule.manage", "task.submit", "task.read", "resource.read", "event.read", "system.status"],
            "units": 1, "ttl_seconds": 90, "tool_targets": ["profile-local", state_id],
        })
        schedule = node.call("Schedule.Create", {
            "key": key(), "tool": "system.status", "input": {},
            "delegation_id": schedule_grant["delegation_ref"]["resource_id"],
            "event": {"source_ref": source, "condition": {"kind": "LifecycleEntered", "lifecycle": "Active"},
                      "expires_at": timestamp(30), "max_triggers": 1},
        })
        schedule_query = {"id": schedule["reference"]["resource_id"]}
        watch = node.call("Resource.List", {"kind": "HostServiceState", "id": state_id})["watch_start_cursor"]

        def rule():
            return node.call("Schedule.Get", schedule_query)["state"]

        seq_before = position()
        node.stop()
        sqlexec("""CREATE TRIGGER fail_observation_receipt BEFORE INSERT ON versions
                WHEN NEW.kind='Artifact' BEGIN SELECT RAISE(ABORT,'test state receipt rollback'); END""")
        subprocess.run(ctl + ["start", unit], check=True, timeout=15)
        node.start()
        failed = node.wait(submit())
        assert failed["lifecycle"] == "Failed" and failed["conditions"][0]["type"] == "StorageUnavailable", failed
        assert node.get(state_id)["reference"] == source and not rule()["trigger_refs"]
        assert position() == seq_before
        assert not node.call("Resource.Watch", {"cursor": watch})["events"]
        node.stop(signal.SIGKILL)
        sqlexec("DROP TRIGGER fail_observation_receipt")
        node.start()

        active = observe()
        published = active["service_state_ref"]
        assert active["projection"] == "Published" and node.get(state_id)["value"]["lifecycle"] == "Active", active
        seq_active = position()
        event_ref, = eventually(lambda: rule()["trigger_refs"])
        trigger = node.call("Schedule.TriggerGet", {"id": event_ref["resource_id"]})["state"]
        task = node.wait(trigger["task_receipt"]["task_id"])
        assert task["lifecycle"] == "Succeeded", task
        event, = node.call("Resource.Watch", {"cursor": watch})["events"]
        assert (event["event_id"], event["resource_ref"]) == (trigger["source_event"]["event_id"], published)
        with ThreadPoolExecutor(max_workers=3) as pool:
            same = list(pool.map(lambda _: observe(), range(3)))
        assert all(obs["projection"] in ("Unchanged", "Superseded") and obs["service_state_ref"] == published for obs in same), same
        assert position() > seq_active
        assert node.get(service_id)["reference"] == original
        node.stop(signal.SIGKILL)
        node.start()
        assert node.get(state_id)["reference"] == published and rule()["trigger_refs"] == [event_ref]
        subprocess.run(ctl + ["stop", unit], check=True, timeout=15)
        path.unlink()
        subprocess.run(ctl + ["daemon-reload"], check=True, timeout=10)
        missing = observe()
        assert missing["projection"] == "Published" and missing["observation"]["load_state"] == "not-found", missing
        assert node.get(state_id)["value"]["lifecycle"] == "Missing"
        assert rule()["trigger_refs"] == [event_ref]
        node.call("Grant.Revoke", target(grant["grant_ref"]))
        refuses("PermissionDenied", submit)
        node.stop()
        config["services"] = []
        node.config.write_text(json.dumps(config))
        node.start()
        assert node.get(state_id)["value"]["lifecycle"] == "Retired"
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] >= 36 and not after["migration_required"], after
        print("PASS real service state, atomic rollback, concurrent observation, event-triggered Task and recovery")
    finally:
        if path.exists():
            subprocess.run(ctl + ["stop", unit], check=False, timeout=15)
        path.unlink(missing_ok=True)
        subprocess.run(ctl + ["daemon-reload"], check=False, timeout=10)
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin")
    main(parser.parse_args().previous_bin)
