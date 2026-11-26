"""Recover an actual Docker create whose successful response never reached Core."""
from contextlib import ExitStack, closing, contextmanager
from pathlib import Path
import json
import signal
import sqlite3
import subprocess
from urllib.parse import parse_qs

from oci_recovery import endpoint, refusal
from oci_unstarted import proxy
from support import Node, key, target

INSPECT = "application.inspect_start_create"
CLEANUP = "application.discard_start_create"


@contextmanager
def on_proxy(node, name, match, *, status, forward):
    settings = json.loads(node.config.read_text())
    node.stop()
    try:
        with ExitStack() as stack:
            socket = node.base / name
            barrier = proxy(stack, socket, settings.get("docker_socket", "/var/run/docker.sock"),
                            match, status=status, forward=forward)
            try:
                node.config.write_text(json.dumps(dict(settings, docker_socket=str(socket))))
                node.start()
                yield barrier
            finally:
                barrier.release.set()
                node.stop()
    finally:
        node.config.write_text(json.dumps(settings))
    node.start()


def verify_lost_create(binaries, docker, spec, app_ids, *, lose_removal=False, externally_removed=False):
    n = Node(binaries)
    name = ("lost-create-no-candidate" if externally_removed else
            "lost-create-lost-removal" if lose_removal else "lost-create")
    try:
        volume = n.perform("volume.create", {"name": name})
        volume = n.perform("volume.write", dict(**target(volume), name="evidence", content="original data"))["volume_ref"]
        app = n.perform("application.install", dict(spec, name=name, volume=target(volume)))
        app_id, volume_id = app["resource_id"], volume["resource_id"]
        app_ids.append(app_id)
        settings = json.loads(n.config.read_text())
        n.stop()
        n.config.write_text(json.dumps(dict(settings, queue_limit=1, recovery_queue_limit=1)))
        n.start()
        matching = docker + ["ps", "-aq", "--no-trunc", "--filter", f"label=io.psios.application={app_id}"]
        with on_proxy(n, "create-cut.sock", lambda req: endpoint(req,b"POST",b"/containers/create") is not None,
                      status=b" 201 ", forward=False) as created:
            task_id = n.submit("application.start",target(app))
            n.approve(task_id)
            assert created.confirmed.wait(15), "Docker never completed the original create"
            cid, = subprocess.check_output(matching,text=True,timeout=10).split()
            container, = json.loads(subprocess.check_output(docker+["inspect",cid],timeout=10))
            assert container["State"]["Status"] == "created",container
            n.stop(signal.SIGKILL)

        lost = n.wait(task_id)
        assert (lost["lifecycle"],lost["outcome_knowledge"]) == ("Paused","Unknown"),lost
        op, = lost["operation_refs"]
        op_id = op["resource_id"]
        private_intent = "oci-create-intent-" + op_id

        def holds():
            with closing(sqlite3.connect(n.state/"state.db",timeout=5)) as db:
                return dict(db.execute("SELECT volume,holder FROM holds WHERE volume IN (?,?)",(app_id,volume_id)))

        with closing(sqlite3.connect(n.state/"state.db",timeout=5)) as db:
            intent, = db.execute("SELECT body FROM current WHERE id=?",(private_intent,)).fetchone()
            assert not db.execute("SELECT 1 FROM current WHERE id=?",("oci-start-"+op_id,)).fetchone()
        intent = json.loads(intent)
        nonce = intent["attempt"]
        assert intent["phase"]=="CreateIssued" and "container_id" not in intent,intent
        assert container["Config"]["Labels"]["io.psios.create-attempt"]==nonce, container
        refusal("PermissionDenied", lambda: n.get(private_intent))
        expected_holds = {app_id:op_id,volume_id:op_id}
        assert holds()==expected_holds
        assert n.call("Node.Get",{})["admission"]["ordinary"]=={"used":1,"limit":1}

        def grant(tool, scope):
            return n.call("Grant.Issue",{"principal_id":"local-owner","actions":["task.submit","task.read",tool],
                                       "tool_targets":scope,"units":8,"ttl_seconds":180})

        def submit(tool,data,grant_):
            request = n.prepare(tool,data,delegation=grant_["delegation_ref"]["resource_id"])
            return n.call("Task.Submit",request)["task_id"]

        def inspect(grant_, wanted="Succeeded"):
            task = n.wait(submit(INSPECT,target(op),grant_))
            assert task["lifecycle"] == wanted, task
            if wanted != "Succeeded":
                assert task["result"]["artifact_refs"]==[],task
                return task
            ref, = task["result"]["artifact_refs"]
            value = n.call("Artifact.Read",{"reference":ref})
            assert nonce not in json.dumps(value),value
            return ref,value

        def reconcile(target_ref, proof):
            return n.call("Operation.Reconcile",dict(**target(target_ref),evidence_ref=proof))

        scope = [op_id,app_id,volume_id]
        rig = grant(INSPECT,scope)
        wrong_scope = grant(INSPECT,scope[:2])
        refusal("PermissionDenied",lambda: submit(INSPECT,target(op),wrong_scope))
        wrong_name = "psios-test-renamed-" + key()
        subprocess.run(docker+["rename",cid,wrong_name],check=True,timeout=10)
        try:
            wrong = inspect(rig, "Failed")
            assert wrong["conditions"][0]["type"] == "OutcomeUnknown", wrong
        finally:
            subprocess.run(docker+["rename",cid,f"psios-{app_id}"],check=True,timeout=10)
        evidence, owned = inspect(rig)
        assert owned["status"]=="CreatedNeverStarted" and owned["container_id"]==cid,owned
        refusal("OutcomeUnknown", lambda: reconcile(op, evidence))
        if externally_removed:
            subprocess.run(docker + ["rm", cid], check=True, stdout=subprocess.DEVNULL, timeout=10)
            empty, observed = inspect(rig)
            assert observed["status"] == "UnverifiedAbsence", observed
            refusal("OutcomeUnknown", lambda: reconcile(op, empty))
            assert holds() == expected_holds
            assert n.wait(task_id) == lost
            print("Verified absence of a muted original CREATE remains unknown", flush=True)
            return
        if not lose_removal:
            config = json.loads(n.config.read_text())
            n.stop()
            n.config.write_text(json.dumps(dict(config, docker_socket=str(n.base / "absent-engine.sock"))))
            try:
                n.start()
                assert inspect(rig, "Failed")["conditions"][0]["type"] == "DependencyUnavailable"
                assert holds() == expected_holds
            finally:
                n.stop()
                n.config.write_text(json.dumps(config))
                n.start()
        approval = {"target":target(app),"operation_ref":op,"evidence_ref":evidence}
        wrong_disposal = grant(CLEANUP,scope[:2])
        refusal("PermissionDenied",lambda: submit(CLEANUP,approval,wrong_disposal))

        revoked = grant(CLEANUP,scope)
        # Hold the actual container inspect response during the cleanup worker's
        # live recheck. Revocation must then fence the not-yet-issued DELETE.
        inspect_path = f"/containers/{cid}/json".encode()
        with on_proxy(n,"pre-remove-authorization.sock",lambda req: endpoint(req,b"GET",inspect_path) is not None,
                      status=b" 200 ", forward=True) as before_remove:
            unstarted = submit(CLEANUP,approval,revoked)
            assert n.wait(unstarted)["lifecycle"]=="WaitingApproval"
            n.approve(unstarted)
            assert before_remove.confirmed.wait(15),"worker did not inspect the exact candidate"
            n.call("Grant.Revoke",target(revoked["grant_ref"]))
            before_remove.release.set()
            denied = n.wait(unstarted)
            assert denied["outcome_knowledge"]=="NotStarted",denied
            assert subprocess.check_output(matching,text=True,timeout=10).split()==[cid]
            assert holds()==expected_holds

        cleanup_grant = grant(CLEANUP,scope)
        seen = set()

        def removal(req):
            assert endpoint(req,b"POST",b"/start") is None
            cut = endpoint(req,b"DELETE",f"/containers/{cid}".encode())
            if cut is not None:
                seen.add((cut.path,cut.query))
            return cut is not None

        with on_proxy(n,"remove-cut.sock",removal,status=b" 204 ",forward=not lose_removal) as removing:
            cleanup_id = submit(CLEANUP,approval,cleanup_grant)
            assert n.wait(cleanup_id)["lifecycle"]=="WaitingApproval"
            refusal("Conflict", lambda: submit(CLEANUP, approval, cleanup_grant))
            assert seen==set() and holds()==expected_holds
            n.approve(cleanup_id)
            assert removing.confirmed.wait(15),"Docker removal was not delivered"
            path, query = next(iter(seen))
            assert path.endswith(f"/containers/{cid}".encode()) and len(seen)==1,seen
            assert not any({b"1",b"true"}.intersection(parse_qs(query).get(flag,[]))
                           for flag in (b"force",b"v")),query
            if lose_removal:
                n.stop(signal.SIGKILL)
            else:
                removing.release.set()
                assert n.wait(cleanup_id)["lifecycle"]=="Succeeded"
        assert holds()==expected_holds
        assert not subprocess.check_output(matching,timeout=10).strip()
        refused = n.wait(cleanup_id)
        refusal("OutcomeUnknown", lambda: submit(CLEANUP, approval, cleanup_grant))
        if lose_removal:
            assert (refused["lifecycle"],refused["outcome_knowledge"])==("Paused","Unknown"),refused
            cleanup_op, = refused["operation_refs"]
            absent, observed = inspect(rig)
            assert observed["status"]=="RemovalConfirmed" and observed["container_id"]==cid,observed
            refusal("Conflict",lambda: reconcile(op,absent))
            assert reconcile(cleanup_op,absent)["outcome_knowledge"]=="Known"
        else:
            receipt, = refused["result"]["artifact_refs"]
            refusal("OutcomeUnknown",lambda: reconcile(op,receipt))
            absent, observed = inspect(rig)
            assert observed["status"]=="RemovalConfirmed",observed
        assert holds()==expected_holds
        assert reconcile(op,absent)["outcome_knowledge"]=="Known"
        final = n.wait(task_id)
        assert (final["lifecycle"],final["outcome_knowledge"])==("Failed","Known"),final
        assert not holds()
        assert n.perform("volume.read",dict(**target(volume),name="evidence"))["content"]=="original data"
        n.stop()
        subprocess.run(n.daemon+["verify"],check=True,stdout=subprocess.DEVNULL,timeout=15)
        print(f"Verified independently authorized OCI lost-create compensation ({name})",flush=True)
    finally:
        n.close()


def verify_create_format_upgrade(previous, current, docker, spec, app_ids):
    n = Node(previous)
    try:
        volume = n.perform("volume.create", {"name": "legacy-create-data"})
        app = n.perform("application.install", dict(spec, name="legacy-create", volume=target(volume)))
        app_id = app["resource_id"]
        app_ids.append(app_id)
        with on_proxy(n, "legacy-create.sock", lambda req: endpoint(req, b"POST", b"/containers/create") is not None,
                      status=b" 201 ", forward=False) as created:
            old_task = n.submit("application.start", target(app))
            n.approve(old_task)
            assert created.confirmed.wait(15)
            n.stop(signal.SIGKILL)
        old = n.wait(old_task)
        assert (old["lifecycle"], old["outcome_knowledge"]) == ("Paused", "Unknown"), old
        old_op, = old["operation_refs"]
        old_container, = subprocess.check_output(
            docker+["ps","-aq","--no-trunc","--filter",f"label=io.psios.application={app_id}"],
            text=True,timeout=10).split()
        observed, = json.loads(subprocess.check_output(docker+["inspect",old_container],timeout=10))
        assert "io.psios.create-attempt" not in observed["Config"]["Labels"]

        other_volume = n.perform("volume.create", {"name": "cross-format-create-data"})
        candidate = n.perform("application.install",dict(spec,name="cross-format-create",volume=target(other_volume)))
        app_ids.append(candidate["resource_id"])
        carried = n.submit("application.start",target(candidate))
        assert n.wait(carried)["lifecycle"]=="WaitingApproval"
        n.stop()
        old_daemon = n.daemon.copy()
        n.daemon[0] = str(Path(current).absolute() / "psiosd")
        preview = json.loads(subprocess.check_output(n.daemon+["verify","--rehearse-migration"],timeout=20))
        assert preview["database_format"]==108 and preview["migration_rehearsal"]["target_format"]==109,preview
        n.start()
        assert n.wait(old_task)==old
        scope=[old_op["resource_id"],app_id,volume["resource_id"]]
        grant=n.call("Grant.Issue",{"principal_id":"local-owner","actions":["task.submit","task.read",INSPECT],
                                   "tool_targets":scope,"units":2,"ttl_seconds":120})
        def stale_scope():
            request=n.prepare(INSPECT,target(old_op),delegation=grant["delegation_ref"]["resource_id"])
            return n.call("Task.Submit",request)
        refusal("UnsupportedCapability",stale_scope)
        n.approve(carried)
        started=n.wait(carried)
        assert started["lifecycle"]=="Succeeded",started
        latest=n.get(candidate["resource_id"])
        nonce_container, = json.loads(subprocess.check_output(docker+["inspect",latest["value"]["container_id"]],timeout=10))
        assert len(nonce_container["Config"]["Labels"]["io.psios.create-attempt"])==64
        n.perform("application.stop",target(latest["reference"]))
        n.stop()
        rejected=subprocess.run(old_daemon+["check"],capture_output=True,timeout=10)
        assert rejected.returncode!=0 and b"SchemaVersionUnsupported" in rejected.stderr,rejected
        verified=json.loads(subprocess.check_output(n.daemon+["verify"],timeout=15))
        assert verified["database_format"]==109,verified
        print("Verified Docker create recovery fencing on exact format 108 to 109",flush=True)
    finally:
        n.close()
