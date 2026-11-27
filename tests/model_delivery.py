"""A local delivery barrier. All protocol replies come from the running test authority."""
from contextlib import contextmanager
import json
import socket
import struct
import subprocess


def frame(peer):
    def read(size):
        body = bytearray()
        while len(body) < size:
            chunk = peer.recv(size - len(body))
            if not chunk:
                raise EOFError("worker connection closed")
            body.extend(chunk)
        return bytes(body)
    prefix = read(4)
    length, = struct.unpack(">I", prefix)
    assert 0 < length <= 1024 * 1024
    return prefix + read(length)


def interrupt_delivery(node, token_file, session, key, directory, *, committed, local_model=None, binary=None):
    args = ["model-step", session, "--key", key, "--decision-dir", directory]
    if local_model is not None:
        args.extend(["--local-model", local_model])
    return interrupt_worker_delivery(node, token_file, args, committed=committed, binary=binary,
                                     timeout=45 if local_model else 10)


@contextmanager
def interrupt_worker_delivery(node, token_file, args, *, committed, timeout=10, method="Model.Respond", binary=None):
    endpoint = node.base / "delivery.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(endpoint))
        listener.listen()
        listener.settimeout(timeout)
        command = [binary or node.binaries / "psiosctl", "--socket", endpoint, "--token-file", token_file, *args]
        with subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as worker:
            try:
                while True:
                    peer, _ = listener.accept()
                    with peer:
                        peer.settimeout(10)
                        request = frame(peer)
                        request_method = json.loads(request[4:])["method"]
                        decision = request_method == method
                        if not decision or committed:
                            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream:
                                upstream.settimeout(10)
                                upstream.connect(str(node.socket))
                                upstream.sendall(request)
                                reply = frame(upstream)
                            if not decision:
                                try:
                                    peer.sendall(reply)
                                except (BrokenPipeError, ConnectionResetError):
                                    # The provider can end its read-only status poll while
                                    # its decision still needs the original delivery barrier.
                                    if request_method != "Model.LeaseStatus":
                                        raise
                                continue
                            assert "result" in json.loads(reply[4:])
                        yield
                        break
            finally:
                worker.kill()
                worker.wait(10)
                endpoint.unlink()
