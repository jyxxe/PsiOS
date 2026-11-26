"""Exercise the OCI catalog boundary against real Docker and SQLite transactions."""
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time

from oci_recovery import endpoint, stop_proxy
from support import Node, target


def image_inspection(request):
    path = endpoint(request, b"GET", b"/json")
    return path is not None and b"/images/" in path.path


def committed_catalog_witness(node, stage_input):
    original = node.prepare("application.stage_release", stage_input)
    task = node.call("Task.Submit", original)["task_id"]
    assert node.wait(task)["lifecycle"] == "WaitingApproval"
    daemon = node.daemon
    witness_path = node.witness / "head"
    trace = node.base / "catalog-witness.strace"
    node.stop()
    node.daemon = ["strace", "-D", "-f", "-qq", "-o", str(trace), "-P", str(witness_path),
                   "-e", "trace=rename,renameat,renameat2",
                   "-e", "inject=rename,renameat,renameat2:delay_enter=500ms", "--", *daemon]
    try:
        node.start()
    finally:
        node.daemon = daemon
    assert os.path.samefile(f"/proc/{node.process.pid}/exe", daemon[0])
    node.approve(task)
    deadline = time.monotonic() + 10
    db_path = (node.state / "state.db").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(db_path, uri=True)) as db:
        while True:
            head, phase, body, spent = db.execute("""
                SELECT m.value,q.phase,t.body,b.spent FROM queue q
                JOIN current t ON t.id=q.task JOIN budgets b ON b.id=q.budget
                JOIN meta m ON m.key='head' WHERE q.task=?
            """, (task,)).fetchone()
            witness = json.loads(witness_path.read_text())
            if phase == "Done" and witness.get("pending") == head and witness["committed"] != head:
                stored = json.loads(body)
                node.stop(signal.SIGKILL)
                break
            assert time.monotonic() < deadline, (phase, witness, trace.read_text())
            time.sleep(0.015)
    assert json.loads(witness_path.read_text()) == witness, witness
    assert '"'+str(witness_path)+'"' in trace.read_text(), trace.read_text()
    assert (stored["lifecycle"], stored["outcome_knowledge"]) == ("Succeeded", "Known"), stored
    report = json.loads(subprocess.check_output(daemon + ["verify"], timeout=20))
    assert report["witness"] == "PendingCommitFinalization", report
    node.start()
    assert node.wait(task) == stored
    with closing(sqlite3.connect(db_path, uri=True)) as db:
        current_spent, = db.execute("SELECT b.spent FROM queue q JOIN budgets b ON b.id=q.budget WHERE q.task=?",
                                    (task,)).fetchone()
    assert current_spent == spent
    replay = node.call("Task.Submit", original)
    assert replay["deduplicated"] and replay["task_id"] == task, replay
    reference, = stored["result"]["artifact_refs"]
    release = node.call("Artifact.Read", {"reference": reference})
    assert node.call("Resource.Resolve", {"reference": release})["value"]["release_version"] == stage_input["release"]["release_version"]
    print("Recovered one committed OCI catalog receipt across the final authority witness cut", flush=True)


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
    if os.getenv("PSIOS_TEST_OCI_CATALOG_CUT") == "1":
        committed_catalog_witness(node, {**stage_input, "release": dict(release, release_version="3.0.0")})
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


def verify_catalog_upgrade(previous, current, spec, app_ids):
    node = Node(previous)
    try:
        install = dict(spec, name="catalog-upgrade", environment={"EPHEMERAL_ONLY": "1", "TEST_NAME": "upgrade"})
        del install["volume"]
        install_task = node.submit("application.install", install)
        original = node.wait(install_task, autoapprove=True)
        assert original["lifecycle"] == "Succeeded", original
        artifact, = original["result"]["artifact_refs"]
        app = node.call("Artifact.Read", {"reference": artifact})
        app_ids.append(app["resource_id"])
        old_daemon = node.daemon.copy()
        node.stop(signal.SIGKILL)
        old = json.loads(subprocess.check_output(old_daemon + ["verify"], timeout=20))
        assert old["database_format"] == 123, old
        node.daemon[0] = str(Path(current).absolute() / "psiosd")
        before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        assert before["node_id"] == old["node_id"] and before["migration_required"], before

        settings = json.loads(node.config.read_text())
        args = {"target": target(app), "release": dict(install, release_version="2.0.0")}
        with stop_proxy(node, settings, "catalog-old-writer", image_inspection,
                        forward=True, status=b" 200 ") as barrier:
            assert node.wait(install_task) == original
            assert node.call("Artifact.Read", {"reference": artifact}) == app
            request = node.prepare("application.stage_release", args)
            task = node.call("Task.Submit", request)["task_id"]
            assert node.wait(task)["lifecycle"] == "WaitingApproval"
            node.approve(task)
            assert barrier.confirmed.wait(12), "Docker did not return the approved image inspection"
            active = node.call("Task.Get", {"id": task})
            [op] = active["operation_refs"]
            [effect] = node.call("Resource.Resolve", {"reference": op})["value"]["effect_refs"]
            assert node.call("Resource.Resolve", {"reference": effect})["value"]["phase"] == "PermitBound"
            node.stop(signal.SIGKILL)
        old_writer = subprocess.run(old_daemon + ["serve"], capture_output=True, text=True, timeout=15)
        assert old_writer.returncode != 0 and "SchemaVersionUnsupported" in old_writer.stderr, old_writer
        node.start()
        task_state = node.wait(task)
        assert (task_state["lifecycle"], task_state["outcome_knowledge"]) == ("Failed", "NotStarted"), task_state
        assert task_state["operation_refs"][0]["resource_id"] == op["resource_id"]
        assert node.get(app["resource_id"])["reference"] == app
        replay = node.call("Task.Submit", request)
        assert replay["deduplicated"] and replay["task_id"] == task, replay
        node.perform("application.stage_release", args)
        node.perform("application.uninstall", target(app))
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=20))
        assert after["database_format"] >= 124 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"], after
        print("PASS format 123 installs retained and old writer fenced while a new catalog effect was pending", flush=True)
    finally:
        node.close()
