"""Drop a committed Docker start or removal receipt before the worker can settle it."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import socketserver
import sqlite3
import subprocess
import threading
from urllib.parse import parse_qs, urlsplit

from support import Failure, key, target


def kernel_group(docker, container):
    inspect = json.loads(subprocess.check_output(docker + ["inspect", container]))[0]
    membership = Path(f"/proc/{inspect['State']['Pid']}/cgroup").read_text().strip()
    assert membership.startswith("0::/") and "\n" not in membership, membership
    root = Path("/sys/fs/cgroup")
    group = (root / membership[4:]).resolve(strict=True)
    assert group.is_relative_to(root) and group.name in {container, f"docker-{container}.scope"}, group
    return group, inspect


def check_kernel_limits(observed, docker, container):
    group, inspect = kernel_group(docker, container)
    limits = observed["effective_limits"]
    quota, period = map(int, (group / "cpu.max").read_text().split())
    status = dict(line.split(":", 1) for line in Path(f"/proc/{inspect['State']['Pid']}/status").read_text().splitlines())
    assert limits == {
        "source": "LinuxCgroupV2", "host_pid": inspect["State"]["Pid"], "cgroup_inode": str(group.stat().st_ino),
        "cpu_quota_us": quota, "cpu_period_us": period, "cpu_burst_us": int((group / "cpu.max.burst").read_text()),
        "memory_max_bytes": int((group / "memory.max").read_text()), "swap_max_bytes": int((group / "memory.swap.max").read_text()),
        "pids_max": int((group / "pids.max").read_text()),
        "process_isolation": {
            "seccomp_mode": int(status["Seccomp"]),
            "no_new_privileges": status["NoNewPrivs"].strip() == "1",
            "capability_sets_empty": all(int(status[field], 16) == 0
                                         for field in ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb")),
        },
    }, limits
    assert limits["process_isolation"] == {"seccomp_mode": 2, "no_new_privileges": True, "capability_sets_empty": True}


def kernel_limit_drift(n, app, docker, container):
    group, inspect = kernel_group(docker, container)
    quota, period = map(int, (group / "cpu.max").read_text().split())
    memory = int((group / "memory.max").read_text())
    pids = int((group / "pids.max").read_text())
    alterations = {
        "cpu.max": f"{quota + period // 20} {period}\n",
        "cpu.max.burst": "1\n",
        "memory.max": f"{memory + os.sysconf('SC_PAGESIZE')}\n",
        "memory.swap.max": f"{os.sysconf('SC_PAGESIZE')}\n",
        "pids.max": f"{pids + 1}\n",
    }

    def update(control, value):
        subprocess.run(["sudo", "-n", "tee", str(control)], input=value, text=True, check=True,
                       stdout=subprocess.DEVNULL)

    for name, altered in alterations.items():
        control = group / name
        original = control.read_text()
        update(control, altered)
        try:
            assert control.read_text() == altered, name
            actual = json.loads(subprocess.check_output(docker + ["inspect", container]))[0]
            assert actual["HostConfig"] == inspect["HostConfig"], name
            mismatch = n.wait(n.submit("application.health", target(app)))
            assert mismatch["lifecycle"] == "Failed" and mismatch["conditions"][0]["type"] == "PreconditionFailed", (name, mismatch)
            assert mismatch["result"]["artifact_refs"] == [], (name, mismatch)
        finally:
            update(control, original)


class ReceiptBarrier(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path, upstream, match_request, status=b" 204 ", forward=False):
        self.upstream = upstream
        self.match_request = match_request
        self.status = status
        self.forward = forward
        self.confirmed = threading.Event()
        self.release = threading.Event()
        super().__init__(str(path), Relay)

    def confirm(self, request):
        self.confirmed.set()
        return self.release.wait(20) and self.forward


class Relay(socketserver.BaseRequestHandler):
    def handle(self):
        server = self.server
        with socket.socket(socket.AF_UNIX) as upstream, selectors.DefaultSelector() as sel:
            upstream.connect(server.upstream)
            sel.register(self.request, selectors.EVENT_READ, upstream)
            sel.register(upstream, selectors.EVENT_READ, self.request)
            request_tail = b""
            matched = False
            held = False
            try:
                while True:
                    for event, _ in sel.select():
                        data = event.fileobj.recv(65536)
                        if not data:
                            return
                        if event.fileobj is self.request:
                            request_tail = (request_tail + data)[-8192:]
                            matched |= server.match_request(request_tail)
                        elif matched and not held and server.status in data:
                            if not server.confirm(request_tail):
                                return
                            held = True
                        event.data.sendall(data)
            except (BrokenPipeError, ConnectionResetError):
                return


def lose_receipt(n, app, volume, tool, match_request, previous_binaries=None):
    before = n.wait(n.submit("application.health", target(app)))
    stale = before["result"]["artifact_refs"][0]
    original = json.loads(n.config.read_text())
    real_socket = original.get("docker_socket", "/var/run/docker.sock")
    path = n.base / (tool + ".sock")
    with ReceiptBarrier(path, real_socket, match_request) as barrier:
        serving = threading.Thread(target=barrier.serve_forever, daemon=True)
        serving.start()
        try:
            n.stop()
            n.config.write_text(json.dumps(dict(original, docker_socket=str(path))))
            n.start()
            task = n.submit(tool, target(app))
            if tool == "application.start":
                n.approve(task)
            assert barrier.confirmed.wait(15), f"engine did not confirm {tool}"
            n.stop(signal.SIGKILL)
        finally:
            barrier.release.set()
            barrier.shutdown()
            serving.join(3)
    # The original holds must survive both the crash and an unavailable engine.
    n.config.write_text(json.dumps(dict(original, docker_socket=str(n.base / "missing-engine.sock"))))
    n.start()
    lost = n.wait(task)
    assert (lost["lifecycle"], lost["outcome_knowledge"]) == ("Paused", "Unknown"), lost
    op = lost["operation_refs"][0]
    try:
        n.call("Operation.Reconcile", dict(**target(op), evidence_ref=stale))
    except Failure as error:
        assert error.code == "OutcomeUnknown", error
    else:
        raise AssertionError("observation taken before the fault was accepted")
    unavailable = n.wait(n.submit("application.health", target(app)))
    assert unavailable["lifecycle"] == "Failed", unavailable
    still_bound = n.get(volume["resource_id"])["reference"]
    busy = n.wait(n.submit("backup.create", target(still_bound)))
    assert busy["conditions"][0]["type"] == "Conflict", busy
    n.stop()
    n.config.write_text(json.dumps(original))
    n.start()
    if tool == "application.start":
        docker = ["docker", "--host", "unix://" + real_socket]
        cid = subprocess.check_output(docker + ["ps", "-q", "--no-trunc", "--filter",
            "label=io.psios.operation=" + op["resource_id"]], text=True).strip()
        limits = json.loads(subprocess.check_output(docker + ["inspect", "--format", "{{json .HostConfig}}", cid]))
        budget = ["update", f"--memory={limits['Memory']}"]
        changed_swap = limits["MemorySwap"] + 16 * 1024 * 1024
        subprocess.run(docker + budget + [f"--memory-swap={changed_swap}", cid], check=True, stdout=subprocess.DEVNULL)
        try:
            mismatch = n.wait(n.submit("application.health", target(app)))
            assert mismatch["lifecycle"] == "Failed" and mismatch["conditions"][0]["type"] == "PreconditionFailed", mismatch
            assert mismatch["result"]["artifact_refs"] == [], mismatch
        finally:
            subprocess.run(docker + budget + [f"--memory-swap={limits['MemorySwap']}", cid], check=True,
                           stdout=subprocess.DEVNULL)
        if os.getenv("PSIOS_TEST_OCI_KERNEL_LIMITS"):
            kernel_limit_drift(n, app, docker, cid)
        subprocess.run(docker + ["pause", cid], check=True, stdout=subprocess.DEVNULL)
        try:
            paused = n.wait(n.submit("application.health", target(app)))
            assert paused["lifecycle"] == "Succeeded", paused
            sample = paused["result"]["artifact_refs"][0]
            assert not n.call("Artifact.Read", {"reference": sample})["process_running"]
            try:
                n.call("Operation.Reconcile", dict(**target(op), evidence_ref=sample))
            except Failure as error:
                assert error.code == "OutcomeUnknown", error
            else:
                raise AssertionError("paused instance accepted as a running postcondition")
        finally:
            subprocess.run(docker + ["unpause", cid], check=True, stdout=subprocess.DEVNULL)
        if previous_binaries:
            current = n.daemon[0]
            n.stop()
            n.daemon[0] = str(Path(previous_binaries).resolve() / "psiosd")
            try:
                n.start()
                legacy = n.wait(n.submit("application.health", target(app)))
                assert legacy["lifecycle"] == "Succeeded", legacy
                legacy_ref, = legacy["result"]["artifact_refs"]
                legacy_limits = n.call("Artifact.Read", {"reference": legacy_ref}).get("effective_limits") or {}
                assert "process_isolation" not in legacy_limits, legacy_limits
            finally:
                n.stop()
                n.daemon[0] = current
                n.start()
            try:
                n.call("Operation.Reconcile", dict(**target(op), evidence_ref=legacy_ref))
            except Failure as error:
                assert error.code == "OutcomeUnknown", error
            else:
                raise AssertionError("legacy observation settled a pending start without a fresh limits check")
    proof = n.wait(n.submit("application.health", target(app)))
    assert proof["lifecycle"] == "Succeeded", proof
    evidence = proof["result"]["artifact_refs"][0]
    observed = n.call("Artifact.Read", {"reference": evidence})
    assert observed["engine_id"] and observed["container_id"]
    if tool == "application.start":
        assert observed["process_running"] and observed["start_operation_id"] == op["resource_id"]
        if os.getenv("PSIOS_TEST_OCI_KERNEL_LIMITS"):
            check_kernel_limits(observed, docker, cid)
    else:
        assert observed["removal_confirmed"]
    try:
        n.call("Operation.Reconcile", dict(**target(op), evidence_ref=still_bound))
    except Failure as error:
        assert error.code == "PreconditionFailed", error
    else:
        raise AssertionError("non-observation accepted as proof")
    n.call("Operation.Reconcile", dict(**target(op), evidence_ref=evidence))
    done = n.wait(task)
    result = n.call("Artifact.Read", {"reference": done["result"]["artifact_refs"][0]})
    assert (done["lifecycle"], done["outcome_knowledge"]) == ("Succeeded", "Known")
    artifact = n.get(done["result"]["artifact_refs"][0]["resource_id"])
    record = n.call("Resource.Resolve", {"reference": artifact["resource"]["spec"]["reconciliation_evidence_ref"]})
    assert record["value"]["evidence"]["observation_ref"] == evidence
    assert result == n.get(app["resource_id"])["reference"]
    attachments = n.get(volume["resource_id"])["value"]["attachment_refs"]
    if tool == "application.start":
        assert len(attachments) == 1
        instance = n.get(n.get(app["resource_id"])["value"]["instance_ref"]["resource_id"])
        assert instance["resource"]["spec"]["container_id"] == observed["container_id"]
        assert instance["resource"]["spec"]["start_operation_id"] == op["resource_id"]
        duplicate = n.call("Task.Get", {"id": task})
        assert duplicate["operation_refs"] == done["operation_refs"]
        try:
            n.call("Operation.Reconcile", dict(**target(duplicate["operation_refs"][0]), evidence_ref=evidence))
        except Failure as error:
            assert error.code == "PreconditionFailed", error
        else:
            raise AssertionError("settled start accepted twice")
    else:
        assert attachments == []
    return result, observed


def lose_start_receipt(n, app, volume, previous_binaries=None):
    def start(request):
        return b"POST " in request and b"/containers/" in request and b"/start HTTP/" in request
    result, _ = lose_receipt(n, app, volume, "application.start", start, previous_binaries)
    return result


def lose_stop_receipt(n, app, cid, volume):
    def remove(request):
        return b"DELETE " in request and b"/containers/" + cid.encode() in request
    result, observed = lose_receipt(n, app, volume, "application.stop", remove)
    assert observed["container_id"] == cid
    return result


@contextmanager
def stop_proxy(node, settings, name, match, *, forward=False, status=b" 204 "):
    socket_path = node.base / (name + ".sock")
    upstream = settings.get("docker_socket", "/var/run/docker.sock")
    with ReceiptBarrier(socket_path, upstream, match, status=status, forward=forward) as barrier:
        server = threading.Thread(target=barrier.serve_forever, daemon=True)
        server.start()
        try:
            node.stop()
            node.config.write_text(json.dumps(dict(settings, docker_socket=str(socket_path))))
            node.start()
            yield barrier
        finally:
            barrier.release.set()
            try:
                node.stop()
            finally:
                barrier.shutdown()
                server.join(3)
                node.config.write_text(json.dumps(settings))


def endpoint(request, method, suffix):
    for line in request.splitlines():
        parts = line.split(b" ", 2)
        if len(parts) == 3 and parts[0] == method and parts[2].startswith(b"HTTP/"):
            url = urlsplit(parts[1])
            if url.path.endswith(suffix):
                return url
    return None


def refusal(code, call):
    try:
        call()
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"expected {code}")


def continue_after_lost_stop(node, app, volume, *, lose_removal):
    """Keep exit and removal evidence separate, including two lost receipts."""
    app_id = app["resource_id"]
    volume_id = volume["resource_id"]
    settings = json.loads(node.config.read_text())
    limited = dict(settings, queue_limit=1, recovery_queue_limit=1)
    name = "stop-cont-lost" if lose_removal else "stop-cont-direct"
    tool = "application.continue_stop"

    def health():
        task_id = node.submit("application.health", target(app))
        task = node.wait(task_id)
        assert task["lifecycle"] == "Succeeded", task
        artifact, = task["result"]["artifact_refs"]
        return task_id, artifact, node.call("Artifact.Read", {"reference": artifact})

    def reconciliation(operation, evidence):
        return node.call("Operation.Reconcile", dict(target(operation), evidence_ref=evidence))

    def holds():
        with sqlite3.connect(node.state / "state.db", timeout=5) as db:
            return dict(db.execute("SELECT volume,holder FROM holds WHERE volume IN (?,?)", (app_id, volume_id)))

    _, stale, running = health()
    cid = running["container_id"]
    container_path = f"/containers/{cid}".encode()
    docker = ["docker", "--host", "unix://" + settings.get("docker_socket", "/var/run/docker.sock")]
    assert running["process_running"] and running["started_at"], running
    try:
        with stop_proxy(node, settings, name + "-stop",
                        lambda request: endpoint(request, b"POST", container_path + b"/stop") is not None) as barrier:
            original_task = node.submit("application.stop", target(app))
            assert barrier.confirmed.wait(15), "Docker did not confirm the original stop"
            node.stop(signal.SIGKILL)

        container, = json.loads(subprocess.check_output(docker + ["inspect", cid], timeout=15))
        assert (container["State"]["Status"], container["State"]["StartedAt"]) == ("exited", running["started_at"]), container
        seen = {}

        def trace_removal(request):
            if endpoint(request, b"POST", container_path + b"/stop") is not None:
                seen["stop"] = True
            remove = endpoint(request, b"DELETE", container_path)
            if remove is not None:
                seen["remove"] = parse_qs(remove.query)
            return remove is not None

        with stop_proxy(node, limited, name + "-rm", trace_removal, forward=not lose_removal) as barrier:
            lost = node.wait(original_task)
            assert (lost["lifecycle"], lost["outcome_knowledge"]) == ("Paused", "Unknown"), lost
            original, = lost["operation_refs"]
            original_holds = {app_id: original["resource_id"], volume_id: app_id}
            assert holds() == original_holds
            admission = node.call("Node.Get", {})["admission"]
            assert admission["ordinary"] == {"used": 1, "limit": 1}, admission
            observed_task, exit_ref, exited = health()
            assert node.get(f"recovery-admission-{observed_task}")["value"]["unresolved_operation_ref"] == original
            assert exited["runtime_state"] == "Exited" and exited["exit_confirmed"], exited
            for field in ("container_id", "engine_id", "start_operation_id", "started_at"):
                assert exited[field] == running[field], (field, exited, running)
            grant = node.call("Grant.Issue", {"principal_id": "local-owner",
                "actions": ["task.submit", "task.read", tool], "tool_targets": [app_id, volume_id],
                "units": 4, "ttl_seconds": 120})["delegation_ref"]["resource_id"]

            def request(evidence, delegation=grant):
                return node.prepare(tool, {"target": target(app), "operation_ref": original,
                    "evidence_ref": evidence}, delegation=delegation)

            if not lose_removal:
                refusal("PermissionDenied", lambda: request(exit_ref, "owner-delegation"))
                limited_scope = node.call("Grant.Issue", {"principal_id": "local-owner",
                    "actions": ["task.submit", "task.read", tool], "tool_targets": [app_id],
                    "units": 1, "ttl_seconds": 120})["delegation_ref"]["resource_id"]
                refusal("PermissionDenied", lambda: node.call("Task.Submit", request(exit_ref, limited_scope)))
            refusal("OutcomeUnknown", lambda: node.call("Task.Submit", request(stale)))
            manifest = node.base / f"{name}-continuation.json"
            manifest.write_text(json.dumps({"application_ref": app, "operation_ref": original,
                                            "evidence_ref": exit_ref}))
            submitted = node.cli(node.witness / "owner.token", "application-prepare-stop-continuation",
                                 "--key", key(), "--delegation", grant, str(manifest))
            cleanup = node.call("Task.Submit", submitted)["task_id"]
            assert node.get(f"recovery-admission-{cleanup}")["value"]["unresolved_operation_ref"] == original
            waiting = node.wait(cleanup)
            assert waiting["lifecycle"] == "WaitingApproval" and "remove" not in seen, waiting
            node.approve(cleanup)
            assert barrier.confirmed.wait(15), "Docker did not confirm cleanup removal"
            assert "stop" not in seen, "cleanup issued an additional stop"
            for flag in (b"force", b"v"):
                assert not {b"1", b"true"}.intersection(seen["remove"].get(flag, [])), seen
            if lose_removal:
                node.stop(signal.SIGKILL)
            else:
                barrier.release.set()
                completed = node.wait(cleanup)
                assert (completed["lifecycle"], completed["outcome_knowledge"]) == ("Succeeded", "Known"), completed

        node.start()
        assert "stop" not in seen, "cleanup issued an additional stop"
        assert holds() == original_holds
        assert node.wait(original_task)["outcome_knowledge"] == "Unknown"
        assert node.call("Task.Submit", submitted)["task_id"] == cleanup
        if lose_removal:
            unresolved = node.wait(cleanup)
            assert (unresolved["lifecycle"], unresolved["outcome_knowledge"]) == ("Paused", "Unknown"), unresolved
            cleanup_op, = unresolved["operation_refs"]
            _, absence_ref, absent = health()
            assert absent["removal_confirmed"] and absent["container_id"] == cid, absent
            refusal("Conflict", lambda: reconciliation(original, absence_ref))
            assert holds() == original_holds
            settled = reconciliation(cleanup_op, absence_ref)
            assert settled["outcome_knowledge"] == "Known", settled
            assert node.wait(original_task)["outcome_knowledge"] == "Unknown" and holds() == original_holds

        completed = node.wait(cleanup)
        assert (completed["lifecycle"], completed["outcome_knowledge"]) == ("Succeeded", "Known"), completed
        cleanup_ref, = completed["result"]["artifact_refs"]
        observed = node.call("Artifact.Read", {"reference": cleanup_ref})
        assert observed == {"original_operation_ref": original, "application_ref": app, "container_id": cid,
            "engine_id": running["engine_id"], "start_operation_id": running["start_operation_id"],
            "removal_confirmed": True}, observed
        refusal("OutcomeUnknown", lambda: reconciliation(original, cleanup_ref))
        _, absence_ref, absent = health()
        assert absent["removal_confirmed"] and absent["container_id"] == cid, absent
        recovered = reconciliation(original, absence_ref)
        assert recovered["outcome_knowledge"] == "Known", recovered
        settled = node.wait(original_task)
        assert settled["lifecycle"] == "Succeeded" and settled["operation_refs"][0]["resource_id"] == original["resource_id"], settled
        result_ref, = settled["result"]["artifact_refs"]
        stopped = node.call("Artifact.Read", {"reference": result_ref})
        assert stopped == node.get(app_id)["reference"] and holds() == {}
        assert node.get(volume_id)["value"]["attachment_refs"] == []
        logs = node.perform("application.logs", target(stopped))
        assert logs["availability"] == "Unavailable" and not logs["stdout"] and not logs["stderr"], logs
        print(f"Verified OCI stopped-container continuation ({name}) with a full ordinary queue", flush=True)
        return stopped
    finally:
        node.stop()
        node.config.write_text(json.dumps(settings))
        node.start()
