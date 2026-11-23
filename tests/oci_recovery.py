"""Drop a committed Docker start or removal receipt before the worker can settle it."""
import json
import selectors
import signal
import socket
import socketserver
import subprocess
import threading

from support import Failure, target


class ReceiptBarrier(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path, upstream, match_request):
        self.upstream = upstream
        self.match_request = match_request
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
            try:
                while True:
                    for event, _ in sel.select():
                        data = event.fileobj.recv(65536)
                        if not data:
                            return
                        if event.fileobj is self.request:
                            request_tail = (request_tail + data)[-8192:]
                            matched |= server.match_request(request_tail)
                        elif matched and b" 204 " in data:
                            server.confirmed.set()
                            server.release.wait(20)
                            return
                        event.data.sendall(data)
            except (BrokenPipeError, ConnectionResetError):
                return


def lose_receipt(n, app, volume, tool, match_request):
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
    proof = n.wait(n.submit("application.health", target(app)))
    assert proof["lifecycle"] == "Succeeded", proof
    evidence = proof["result"]["artifact_refs"][0]
    observed = n.call("Artifact.Read", {"reference": evidence})
    assert observed["engine_id"] and observed["container_id"]
    if tool == "application.start":
        assert observed["process_running"] and observed["start_operation_id"] == op["resource_id"]
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


def lose_start_receipt(n, app, volume):
    def start(request):
        return b"POST " in request and b"/containers/" in request and b"/start HTTP/" in request
    result, _ = lose_receipt(n, app, volume, "application.start", start)
    return result


def lose_stop_receipt(n, app, cid, volume):
    def remove(request):
        return b"DELETE " in request and b"/containers/" + cid.encode() in request
    result, observed = lose_receipt(n, app, volume, "application.stop", remove)
    assert observed["container_id"] == cid
    return result
