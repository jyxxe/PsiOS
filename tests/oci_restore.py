"""Live classic Docker acceptance for approved restore and original-operation reconciliation."""
import argparse
import hashlib
import http.client
import http.server
import io
import json
import signal
import socket
import socketserver
import sqlite3
import subprocess
import tarfile
import threading

from oci import corrupted_archive, install_from_artifact, retire_oci_release
from oci_capture import DOCKER, SOCKET, denied
from support import Node, key, target
from test_artifact_install import grant

RESTORE = "application.restore_release_image"
INSPECT = "application.inspect_release_image_restore"


class UnixHTTP(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(SOCKET)


class FlightRelay(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path):
        self.staged = threading.Event()
        self.deliver = threading.Event()
        self.completed = threading.Event()
        self.post = None
        self.response = None
        self.failure = None
        super().__init__(str(path), FlightHandler)


class FlightHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.engine = UnixHTTP("localhost", timeout=8)

    def finish(self):
        self.engine.close()
        super().finish()

    def relay(self):
        try:
            length = int(self.headers.get("Content-Length", "0"))
            assert 0 <= length <= 8 * 1024 * 1024
            command = self.rfile.read(length)
            if self.command == "POST":
                assert self.path == "/v1.44/images/load?quiet=1" and self.server.post is None
                self.server.post = command
                self.server.staged.set()
                assert self.server.deliver.wait(30), "test did not release the checkpointed Docker request"
            headers = {k: self.headers[k] for k in ("Content-Type",) if k in self.headers}
            self.engine.request(self.command, self.path, command or None, headers)
            response = self.engine.getresponse()
            content = response.read()
            if self.command == "POST":
                self.server.response = (response.status, content)
                self.server.completed.set()
            self.send_response(response.status)
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)
        except (BrokenPipeError, ConnectionResetError):
            if not self.server.completed.is_set():
                raise
        except Exception as failure:
            self.server.failure = failure
            self.server.completed.set()
            self.close_connection = True
            raise

    def log_message(self, *_args):
        pass

    do_GET = relay
    do_POST = relay


def assert_state(value, lifecycle, knowledge, condition):
    assert (value["lifecycle"], value["outcome_knowledge"], value["conditions"][0]["type"]) == (
        lifecycle, knowledge, condition), value


def current_image(image):
    response = subprocess.run(DOCKER + ["image", "inspect", image], capture_output=True, timeout=10)
    return json.loads(response.stdout)[0] if response.returncode == 0 else None


def rm_image(image):
    subprocess.run(DOCKER + ["image", "rm", image], check=True, capture_output=True, timeout=15)


def fixture_image(name):
    content = (name + "\n").encode()
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w") as tar:
        member = tarfile.TarInfo("psi-cache-restore-proof")
        member.size = len(content)
        tar.addfile(member, io.BytesIO(content))
    return subprocess.check_output(DOCKER + ["import", "-", name], input=archive.getvalue(),
                                   timeout=20).decode().strip()


def verify_settled_proof(node, result):
    changes = (result["resource_id"], int(result["version"][1:]))
    with sqlite3.connect(node.state / "state.db") as db:
        original, = db.execute("SELECT spec FROM versions WHERE id=? AND rev=?", changes).fetchone()
        db.execute("UPDATE versions SET spec=json_set(spec,'$.effect_id','unrelated-effect') "
                   "WHERE id=? AND rev=?", changes)
    try:
        bad = subprocess.run(node.daemon + ["verify"], capture_output=True, timeout=15)
        assert bad.returncode != 0 and b"OCI restore result is not linked to its confirmed effect" in bad.stderr, bad
    finally:
        with sqlite3.connect(node.state / "state.db") as db:
            db.execute("UPDATE versions SET spec=? WHERE id=? AND rev=?", (original, *changes))
    subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)


def main(binaries):
    node = Node(binaries)
    name = "psios-release-restore:" + key()
    image = None
    relay = None
    thread = None
    try:
        image = fixture_image(name)
        inspect = current_image(image)
        diff_ids = inspect["RootFS"]["Layers"]
        profile = json.loads(subprocess.check_output(DOCKER + ["info", "--format", "{{json .}}"], timeout=15))
        assert profile["Driver"] in ("overlay2", "vfs"), profile["Driver"]
        engine_id = profile["ID"]
        volume = node.perform("volume.create", {"name": "restore-fixture"})
        app, source, archive = install_from_artifact(node, dict(image_id=image, name="restore-fixture",
            release_version="1.0.0", argv=["/unused"], license_expression="0BSD", volume=target(volume),
            cpu_millicores=100, memory_max_bytes=33554432, pids_max=8, ephemeral_max_bytes=1048576))
        release = node.get(app["resource_id"])["resource"]["spec"]["release_ref"]
        spec_ref = node.call("Resource.Resolve", {"reference": release})["value"]["release_spec_ref"]
        trust = node.call("Resource.Resolve", {"reference": spec_ref})["value"]["provenance"]
        trust_ref = trust["trust_evidence_or_admission_policy_ref"]
        signer = node.call("Resource.Resolve", {"reference": trust_ref})["value"]["signature"]["key_ref"]
        owner = "local-owner"
        node_id = node.call("Node.Get", {})["reference"]["resource_id"]
        scope = [ref["resource_id"] for ref in (app, release, source, signer, archive)] + [node_id]
        actions = ["task.submit", "task.read", RESTORE]
        agent = node.call("Agent.Register", {"name": "untrusted-restore"})
        denied(node, "PermissionDenied", "Grant.Issue", {"principal_id": agent["principal_id"],
               "actions": actions, "tool_targets": scope, "units": 8, "ttl_seconds": 180})
        incomplete = grant(node, owner, actions, [item for item in scope if item != archive["resource_id"]])
        allowed = grant(node, owner, actions, scope)
        request = dict(application_ref=app, release_ref=release, image_archive_ref=archive,
                       publisher_key_ref=signer, expected_engine_id=engine_id)
        owner_token = node.witness / "owner.token"

        def prepare(input_value, permission=allowed):
            path = node.base / "restore-request.json"
            path.write_text(json.dumps(input_value))
            return node.cli(owner_token, "application-prepare-image-restore", "--key", key(),
                            "--delegation", permission["delegation_ref"]["resource_id"], path)

        def submit(req):
            return node.call("Task.Submit", req)["task_id"]

        def read(task):
            ref, = task["result"]["artifact_refs"]
            return node.call("Artifact.Read", {"reference": ref}), ref

        rm_image(name)
        assert current_image(image) is None

        denied(node, "PermissionDenied", "Task.Submit", prepare(request, incomplete))
        with corrupted_archive(node, archive):
            damaged = node.wait(submit(prepare(request)), autoapprove=True)
            assert_state(damaged, "Failed", "NotStarted", "RecoveryRequired")
        assert current_image(image) is None
        bad = node.wait(submit(prepare(dict(request, expected_engine_id="wrong-" + key()))), autoapprove=True)
        assert_state(bad, "Failed", "Known", "PreconditionFailed")
        assert current_image(image) is None
        with sqlite3.connect(node.state / "state.db", timeout=10) as db:
            db.execute(f"""CREATE TRIGGER refuse_known_precheck_settlement BEFORE INSERT ON versions
                WHEN NEW.kind='Task' AND NEW.spec->>'task_type'='{RESTORE}'
                 AND NEW.body->'conditions'->0->>'type'='PreconditionFailed'
                BEGIN SELECT RAISE(ABORT, 'injected precheck settlement loss'); END""")
        try:
            damaged = node.wait(submit(prepare(dict(request, expected_engine_id="wrong-" + key()))),
                                autoapprove=True)
            assert_state(damaged, "Failed", "Known", "StorageUnavailable")
        finally:
            with sqlite3.connect(node.state / "state.db", timeout=10) as db:
                db.execute("DROP TRIGGER refuse_known_precheck_settlement")
        assert current_image(image) is None

        ordinary = prepare(request)
        initial = submit(ordinary)
        assert submit(ordinary) == initial
        waiting = node.wait(initial)
        assert waiting["lifecycle"] == "WaitingApproval" and current_image(image) is None
        approval = node.get(waiting["conditions"][0]["message"])["value"]
        plan = node.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
        assert [p["resource_ref"]["resource_id"] for p in plan["preconditions"]] == scope
        state = node.wait(initial, autoapprove=True)
        assert state["lifecycle"] == "Succeeded", state
        restored, restored_ref = read(state)
        assert restored["observation"]["image_diff_ids"] == diff_ids, restored
        assert restored["observation"]["engine_id"] == engine_id, restored
        assert restored["image_archive_ref"] == archive and restored["release_ref"] == release, restored
        assert (current_image(image)["RepoTags"] or []) == []
        assert node.get(app["resource_id"])["reference"] == app
        assert node.get(volume["resource_id"])["reference"] == volume
        node.call("Task.Archive", target(node.get(initial)["reference"]))

        already = node.wait(submit(prepare(request)), autoapprove=True)
        assert_state(already, "Failed", "Known", "PreconditionFailed")
        rm_image(image)
        assert current_image(image) is None

        original = json.loads(node.config.read_text())
        node.stop()
        endpoint = node.base / "restore-flight.sock"
        relay = FlightRelay(endpoint)
        thread = threading.Thread(target=relay.serve_forever, daemon=True)
        thread.start()
        node.config.write_text(json.dumps(dict(original, docker_socket=str(endpoint))))
        node.start()
        task_id = submit(prepare(request))
        assert node.wait(task_id)["lifecycle"] == "WaitingApproval"
        node.approve(task_id)
        assert relay.staged.wait(10), "Core did not dispatch the approved archive"
        assert current_image(image) is None
        with sqlite3.connect(node.state / "state.db") as database:
            checkpoint, = database.execute("SELECT body FROM current WHERE kind='OciImageRestoreCheckpoint' "
                                           "ORDER BY seq DESC LIMIT 1").fetchone()
        checkpoint = json.loads(checkpoint)
        assert checkpoint["approved_archive_digest"] == archive["digest"]
        assert checkpoint["normalized_archive_digest"] == "sha256:" + hashlib.sha256(relay.post).hexdigest()
        with tarfile.open(fileobj=io.BytesIO(relay.post)) as loaded:
            manifest, = json.load(loaded.extractfile("manifest.json"))
            assert not manifest["RepoTags"] and len(loaded.getmembers()) == 2 + len(diff_ids)
            assert "sha256:" + hashlib.sha256(loaded.extractfile(manifest["Config"]).read()).hexdigest() == image
        node.stop(signal.SIGKILL)
        subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)
        node.config.write_text(json.dumps(dict(original, queue_limit=1, recovery_queue_limit=1)))
        node.start()
        lost = node.wait(task_id)
        assert_state(lost, "Paused", "Unknown", "OutcomeUnknown")
        operation, = lost["operation_refs"]
        denied(node, "OutcomeUnknown", "Task.Retry", {**target(node.get(task_id)["reference"]), "key": key()})
        assert node.call("Node.Get", {})["admission"]["ordinary"] == {"used": 1, "limit": 1}
        assert node.get(app["resource_id"])["reference"] == app
        denied(node, "Conflict", "Application.Configure", {**target(app), "environment": {}})

        # This independently authorized observation remains allowed after the key
        # that authorized the already dispatched effect has been revoked.
        node.call("Artifact.PublisherKey.Revoke", {**target(signer), "reason": "verification of historical restore"})
        recovery = grant(node, owner, ["task.submit", "task.read", INSPECT],
                         [operation["resource_id"], *scope])
        operation_path = node.base / "restore-operation.json"
        operation_path.write_text(json.dumps(operation))

        def inspect_after_loss():
            prepared = node.cli(owner_token, "application-prepare-image-restore-inspection", "--key", key(),
                                "--delegation", recovery["delegation_ref"]["resource_id"], operation_path)
            recovery_task_id = submit(prepared)
            inspected = node.wait(recovery_task_id)
            assert inspected["lifecycle"] == "Succeeded", inspected
            admission = node.get(f"recovery-admission-{recovery_task_id}")["value"]
            assert admission["unresolved_operation_ref"] == operation
            return read(inspected)

        absent, evidence = inspect_after_loss()
        assert absent["presence"] == "Absent" and absent["engine_id"] == engine_id, absent
        assert "image_diff_ids" not in absent
        denied(node, "OutcomeUnknown", "Operation.Reconcile", {**target(operation), "evidence_ref": evidence})
        assert node.wait(task_id) == lost

        # The already-dispatched request can complete after client death. Release
        # that one real Docker POST and show that inspection/reconciliation never replay it.
        relay.deliver.set()
        assert relay.completed.wait(10) and relay.failure is None, relay.failure
        assert relay.response[0] == 200 and image.encode() in relay.response[1], relay.response
        after, evidence = inspect_after_loss()
        assert after["presence"] == "Present" and after["image_diff_ids"] == diff_ids, after
        confirmed = node.call("Operation.Reconcile", {**target(operation), "evidence_ref": evidence})
        assert confirmed["operation_id"] == operation["resource_id"] and confirmed["outcome_knowledge"] == "Known"
        complete = node.wait(task_id)
        assert complete["lifecycle"] == "Succeeded", complete
        receipt, _ = read(complete)
        assert receipt["operation_id"] == operation["resource_id"], receipt
        assert receipt["observation"]["original_operation_ref"] == operation, receipt
        assert current_image(image)["RootFS"]["Layers"] == diff_ids
        with sqlite3.connect(node.state / "state.db") as database:
            assert database.execute("SELECT count(*) FROM oci_image_restore_holds").fetchone() == (0,)
        node.stop()
        verify_settled_proof(node, restored_ref)
        node.start()
        retirement = grant(node, owner, ["task.submit", "task.read", "application.retire_release"],
                           [app["resource_id"], release["resource_id"]])
        retire_oci_release(node, app, release, source, archive, retirement, uninstall=True)
        node.stop()
        subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=15)
        print("PASS live approved OCI restore, exact input, absent result refusal and original-operation recovery after SIGKILL")
    finally:
        if relay is not None:
            relay.deliver.set()
            relay.shutdown()
            relay.server_close()
            thread.join(3)
        if image is not None:
            owned = name if current_image(name) is not None else image
            if current_image(owned) is not None:
                rm_image(owned)
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", default="target/debug")
    main(parser.parse_args().bin_dir)
