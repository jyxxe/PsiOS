"""Hold one real Docker removal response after the engine committed it, then kill the authority."""
import json
import selectors
import signal
import socket
import socketserver
import threading

from support import Failure, target


class RemovalBarrier(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path, upstream, cid):
        self.upstream = upstream
        self.cid = cid.encode()
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
            deleting = False
            try:
                while True:
                    for event, _ in sel.select():
                        data = event.fileobj.recv(65536)
                        if not data:
                            return
                        if event.fileobj is self.request:
                            request_tail = (request_tail + data)[-8192:]
                            deleting |= b"DELETE " in request_tail and b"/containers/" + server.cid in request_tail
                        elif deleting and b" 204 " in data:
                            server.confirmed.set()
                            server.release.wait(20)
                            return
                        event.data.sendall(data)
            except (BrokenPipeError, ConnectionResetError):
                return


def lose_stop_receipt(n, app, cid, volume):
    original = json.loads(n.config.read_text())
    real_socket = original.get("docker_socket", "/var/run/docker.sock")
    path = n.base / "engine.sock"
    with RemovalBarrier(path, real_socket, cid) as barrier:
        serving = threading.Thread(target=barrier.serve_forever, daemon=True)
        serving.start()
        try:
            n.stop()
            n.config.write_text(json.dumps(dict(original, docker_socket=str(path))))
            n.start()
            stop = n.submit("application.stop", target(app))
            assert barrier.confirmed.wait(15), "engine did not confirm container removal"
            n.stop(signal.SIGKILL)
        finally:
            barrier.release.set()
            barrier.shutdown()
            serving.join(3)
    # A missing engine cannot certify that a workload or its volume has disappeared.
    n.config.write_text(json.dumps(dict(original, docker_socket=str(n.base / "missing-engine.sock"))))
    n.start()
    lost = n.wait(stop)
    assert (lost["lifecycle"], lost["outcome_knowledge"]) == ("Paused", "Unknown"), lost
    op = lost["operation_refs"][0]
    unavailable = n.wait(n.submit("application.health", target(app)))
    assert unavailable["lifecycle"] == "Failed", unavailable
    still_bound = n.get(volume["resource_id"])["reference"]
    busy = n.wait(n.submit("backup.create", target(still_bound)))
    assert busy["conditions"][0]["type"] == "Conflict", busy
    n.stop()
    n.config.write_text(json.dumps(original))
    n.start()
    proof = n.wait(n.submit("application.health", target(app)))
    assert proof["lifecycle"] == "Succeeded", proof
    evidence = proof["result"]["artifact_refs"][0]
    observed = n.call("Artifact.Read", {"reference": evidence})
    assert observed["removal_confirmed"] and observed["container_id"] == cid and observed["engine_id"]
    try:
        n.call("Operation.Reconcile", dict(**target(op), evidence_ref=still_bound))
    except Failure as error:
        assert error.code == "PreconditionFailed", error
    else:
        raise AssertionError("non-observation accepted as proof")
    n.call("Operation.Reconcile", dict(**target(op), evidence_ref=evidence))
    done = n.wait(stop)
    result = n.call("Artifact.Read", {"reference": done["result"]["artifact_refs"][0]})
    assert (done["lifecycle"], done["outcome_knowledge"]) == ("Succeeded", "Known")
    artifact = n.get(done["result"]["artifact_refs"][0]["resource_id"])
    record = n.call("Resource.Resolve", {"reference": artifact["resource"]["spec"]["reconciliation_evidence_ref"]})
    assert record["value"]["evidence"]["observation_ref"] == evidence
    assert result == n.get(app["resource_id"])["reference"]
    assert n.get(volume["resource_id"])["value"]["attachment_refs"] == []
    return result
