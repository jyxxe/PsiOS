"""Exercise the OCI catalog boundary against real Docker and SQLite transactions."""
from contextlib import closing
import json
import signal
import sqlite3
import subprocess

from oci_recovery import endpoint, stop_proxy
from support import target


def verify_catalog_settlement(node, docker, spec, app_ids):
    db_path = node.state / "state.db"
    volume = node.perform("volume.create", {"name": "oci-catalog-custody"})
    volume = node.perform("volume.write", {**target(volume), "name": "kept", "content": "application data"})["volume_ref"]
    installation = dict(spec, name="catalog-settlement", volume=target(volume),
                        environment={"TEST_NAME": "catalog"})

    def snapshot():
        with closing(sqlite3.connect(db_path)) as db:
            return db.execute("""
                SELECT id,rev,spec,body FROM current WHERE kind IN
                    ('Application','ApplicationRelease','ApplicationReleaseSpec','Deployment','Volume')
                ORDER BY id
            """).fetchall()

    def prepared(tool, request, delegation="owner-delegation"):
        original = node.prepare(tool, request, delegation=delegation)
        task = node.call("Task.Submit", original)["task_id"]
        assert node.wait(task)["lifecycle"] == "WaitingApproval"
        return original, task

    def check_interrupted(task, before, lifecycle, reason, operation=None):
        state = node.wait(task)
        assert (state["lifecycle"], state["outcome_knowledge"]) == (lifecycle, "NotStarted"), state
        if reason is not None:
            assert state["conditions"][0]["type"] == reason, state
        assert state["result"]["artifact_refs"] == [], state
        assert snapshot() == before, "interrupted OCI catalog transaction changed resources"
        [op] = state["operation_refs"]
        if operation is not None:
            assert op["resource_id"] == operation
        effect_ref, = node.call("Resource.Resolve", {"reference": op})["value"]["effect_refs"]
        effect = node.call("Resource.Resolve", {"reference": effect_ref})["value"]
        assert effect["phase"] == "PermitBound" and effect["evidence_refs"] == [], effect
        assert effect["outcome_knowledge"] == "NotStarted", effect
        with closing(sqlite3.connect(db_path)) as db:
            assert db.execute("SELECT phase FROM queue WHERE task=?", (task,)).fetchone() == ("Done",)
            assert db.execute("SELECT volume FROM holds WHERE holder=?", (op["resource_id"],)).fetchall() == []
            assert db.execute("SELECT id FROM current WHERE kind='Artifact' AND spec->>'task_id'=?",
                              (task,)).fetchall() == []
            decision, bound = db.execute("SELECT decision,operation FROM approval_bindings WHERE task=?",
                                         (task,)).fetchone()
            assert bound == op["resource_id"] and decision
            assert db.execute("SELECT body->>'binding_status' FROM current WHERE id=?",
                              (f"use-{decision}",)).fetchone() == ("Bound",)
        return state

    def fail_receipt(tool, request):
        before = snapshot()
        original, task = prepared(tool, request)
        with closing(sqlite3.connect(db_path)) as db, db:
            db.execute(f"""CREATE TRIGGER interrupt_catalog_receipt BEFORE INSERT ON versions
                WHEN NEW.kind='Artifact' AND NEW.spec->>'task_id'='{task}'
                BEGIN SELECT RAISE(ABORT, 'injected OCI catalog receipt failure'); END""")
        try:
            node.approve(task)
            check_interrupted(task, before, "Failed", "StorageUnavailable")
        finally:
            with closing(sqlite3.connect(db_path)) as db, db:
                db.execute("DROP TRIGGER interrupt_catalog_receipt")
        replay = node.call("Task.Submit", original)
        assert replay["deduplicated"] and replay["task_id"] == task, replay

    fail_receipt("application.install", installation)
    app = node.perform("application.install", installation)
    app_id = app["resource_id"]
    app_ids.append(app_id)
    release = dict(installation, release_version="2.0.0")
    del release["volume"]
    stage_input = {"target": target(app), "release": release}
    settings = json.loads(node.config.read_text())

    def image_inspection(request):
        path = endpoint(request, b"GET", b"/json")
        return path is not None and b"/images/" in path.path

    for interruption in ("cancel", "revoke", "kill"):
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 4, "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "application.stage_release"],
            "tool_targets": [app_id],
        })
        before = snapshot()
        with stop_proxy(node, settings, "catalog-" + interruption, image_inspection,
                        forward=True, status=b" 200 ") as barrier:
            original, task = prepared("application.stage_release", stage_input,
                                      grant["delegation_ref"]["resource_id"])
            node.approve(task)
            assert barrier.confirmed.wait(12), "Docker did not return the real image inspection"
            active = node.call("Task.Get", {"id": task})
            [op] = active["operation_refs"]
            [effect] = node.call("Resource.Resolve", {"reference": op})["value"]["effect_refs"]
            assert node.call("Resource.Resolve", {"reference": effect})["value"]["phase"] == "PermitBound"
            with closing(sqlite3.connect(db_path)) as db:
                assert db.execute("SELECT holder FROM holds WHERE volume=?", (app_id,)).fetchone() == (
                    op["resource_id"],)
            if interruption == "kill":
                node.stop(signal.SIGKILL)
            else:
                if interruption == "cancel":
                    node.call("Task.Cancel", target(node.get(task)["reference"]))
                else:
                    node.call("Grant.Revoke", target(grant["grant_ref"]))
                barrier.release.set()
                check_interrupted(task, before, "Cancelled" if interruption == "cancel" else "Failed",
                                  None if interruption == "cancel" else "PermissionDenied", op["resource_id"])
        node.start()
        state = check_interrupted(task, before, "Cancelled" if interruption == "cancel" else "Failed",
                                  "LeaseLost" if interruption == "kill" else None, op["resource_id"])
        if interruption != "revoke":
            replay = node.call("Task.Submit", original)
            assert replay["deduplicated"] and replay["task_id"] == task, replay
        assert node.wait(task) == state

    fail_receipt("application.stage_release", stage_input)
    staged = node.perform("application.stage_release", stage_input)
    selection = {"target": target(app), "release_ref": staged}
    fail_receipt("application.select_release", selection)
    app = node.perform("application.select_release", selection)
    assert node.get(app_id)["resource"]["spec"]["release_ref"] == staged
    app = node.perform("application.start", target(app))
    running = node.perform("application.health", target(app))
    container = running["container_id"]
    assert running["process_running"] and container
    app = node.perform("application.stop", target(app))
    fail_receipt("application.uninstall", target(app))
    app = node.perform("application.uninstall", target(app))
    assert node.get(app_id)["value"]["lifecycle"] == "Uninstalled"
    assert not subprocess.check_output(docker + ["ps", "-aq", "--filter",
                                                 f"label=io.psios.application={app_id}"], text=True).strip()
    assert node.perform("volume.read", {**target(node.get(volume["resource_id"])["reference"]),
                                        "name": "kept"})["content"] == "application data"
    print("PASS real OCI inspection boundaries, approval and atomic catalog state through cancellation, revocation and recovery",
          flush=True)
