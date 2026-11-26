"""Exercise compensation before Start using a real Docker-created container."""
from contextlib import ExitStack, closing
import json
import sqlite3
import subprocess
import threading
from urllib.parse import parse_qs

from oci_recovery import ReceiptBarrier, endpoint
from support import target


def proxy(stack, path, upstream, match, *, status, forward):
    barrier = stack.enter_context(ReceiptBarrier(path, upstream, match, status=status, forward=forward))
    thread = threading.Thread(target=barrier.serve_forever, daemon=True)
    thread.start()

    def stop():
        barrier.release.set()
        barrier.shutdown()
        thread.join(3)

    stack.callback(stop)
    return barrier


def verify_unstarted_rollback(node, docker, app, volume, *, lose_removal=False):
    app_id = app["resource_id"]
    volume_id = volume["resource_id"]
    original = json.loads(node.config.read_text())
    engine = original.get("docker_socket", "/var/run/docker.sock")
    label = ["ps", "-aq", "--no-trunc", "--filter", f"label=io.psios.application={app_id}"]
    requests = {"starts": set(), "removals": set()}
    name = "unstarted-lost" if lose_removal else "unstarted-known"

    def trace_create(request):
        start = endpoint(request, b"POST", b"/start")
        if start is not None:
            requests["starts"].add(start.path)
        remove = endpoint(request, b"DELETE", b"")
        if remove is not None:
            requests["removals"].add((remove.path, remove.query))
        return endpoint(request, b"POST", b"/containers/create") is not None

    node.stop()
    try:
        with ExitStack() as stack:
            dropped = None
            upstream = engine
            if lose_removal:
                upstream = str(node.base / f"{name}-rm.sock")
                dropped = proxy(stack, upstream, engine, lambda req: endpoint(req, b"DELETE", b"") is not None,
                                status=b" 204 ", forward=False)
            socket = node.base / f"{name}-create.sock"
            created = proxy(stack, socket, upstream, trace_create, status=b" 201 ", forward=True)
            try:
                node.config.write_text(json.dumps(dict(original, docker_socket=str(socket))))
                node.start()
                task = node.submit("application.start", target(app))
                node.approve(task)
                assert created.confirmed.wait(12), "Docker did not create the container"
                cid, = subprocess.check_output(docker + label, text=True, timeout=10).split()
                before, = json.loads(subprocess.check_output(docker + ["inspect", cid], timeout=10))
                assert before["Id"] == cid and before["Name"] == f"/psios-{app_id}", before
                assert before["State"]["Status"] == "created" and before["State"]["Pid"] == 0, before
                assert before["State"]["StartedAt"] == "0001-01-01T00:00:00Z", before

                limit = before["HostConfig"]["PidsLimit"] + 1
                subprocess.run(docker + ["update", "--pids-limit", str(limit), cid], check=True,
                               stdout=subprocess.DEVNULL, timeout=10)
                changed, = json.loads(subprocess.check_output(docker + ["inspect", cid], timeout=10))
                assert changed["HostConfig"]["PidsLimit"] == limit, changed
                assert all(changed["State"][field] == before["State"][field]
                           for field in ("Status", "Pid", "StartedAt", "Running")), changed
                created.release.set()
                if dropped is not None:
                    assert dropped.confirmed.wait(12), "Docker did not remove the exact unstarted container"
                    dropped.release.set()
                failed = node.wait(task)
                state = (failed["lifecycle"], failed["outcome_knowledge"], failed["conditions"][0]["type"])
                expected = ("Paused", "Unknown", "OutcomeUnknown") if lose_removal else (
                    "Failed", "Known", "PreconditionFailed")
                assert state == expected, failed
                assert failed["result"]["artifact_refs"] == [], failed
                assert not requests["starts"], requests
                (removed, query), = requests["removals"]
                assert removed.endswith(f"/containers/{cid}".encode()), requests
                flags = parse_qs(query)
                assert all(not {b"1", b"true"}.intersection(flags.get(flag, []))
                           for flag in (b"force", b"v")), requests
                assert not subprocess.check_output(docker + label, timeout=10).strip(), failed
            finally:
                created.release.set()
                if dropped is not None:
                    dropped.release.set()
                node.stop()
    finally:
        node.config.write_text(json.dumps(original))
    node.start()
    assert node.wait(task) == failed, failed
    operation, = failed["operation_refs"]
    with closing(sqlite3.connect(node.state / "state.db", timeout=5)) as db:
        holds = dict(db.execute("SELECT volume,holder FROM holds WHERE volume IN (?,?)", (app_id, volume_id)))
        checkpoint = db.execute("SELECT 1 FROM current WHERE id=?", (f"oci-start-{operation['resource_id']}",)).fetchone()
    assert checkpoint is None, checkpoint
    unchanged = node.get(app_id)
    assert unchanged["reference"] == app and unchanged["value"]["lifecycle"] == "Stopped", unchanged
    current_volume = node.get(volume_id)
    assert current_volume["value"]["attachment_refs"] == [], current_volume
    if lose_removal:
        assert holds == {app_id: operation["resource_id"], volume_id: operation["resource_id"]}, holds
        blocked = node.wait(node.submit("backup.create", target(current_volume["reference"])))
        assert blocked["conditions"][0]["type"] == "Conflict", blocked
    else:
        assert holds == {}, holds
        contents = node.perform("volume.read", dict(**target(current_volume["reference"]), name="starts"))
        assert contents["content"] == "first\nsecond\n", contents
    print(f"Verified OCI rollback before start ({name})", flush=True)
