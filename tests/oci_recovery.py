"""Drop a committed Docker start or removal receipt before the worker can settle it."""
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import socketserver
import subprocess
import threading

from support import Failure, target


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
    assert limits == {
        "source": "LinuxCgroupV2", "host_pid": inspect["State"]["Pid"], "cgroup_inode": str(group.stat().st_ino),
        "cpu_quota_us": quota, "cpu_period_us": period, "cpu_burst_us": int((group / "cpu.max.burst").read_text()),
        "memory_max_bytes": int((group / "memory.max").read_text()), "swap_max_bytes": int((group / "memory.swap.max").read_text()),
        "pids_max": int((group / "pids.max").read_text()),
    }, limits


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
                            server.confirmed.set()
                            if not server.release.wait(20) or not server.forward:
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
                assert "effective_limits" not in n.call("Artifact.Read", {"reference": legacy_ref})
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
