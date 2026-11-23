"""Local system acceptance tests. Run after cargo build --workspace."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import hashlib
import hmac
import base64
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import unittest

from support import Failure, Node, key, target

ACTIONS = ["task.submit", "task.read", "resource.read", "event.read", "system.status"]
BINARIES = "target/debug"


class SystemTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(BINARIES)
        self.addCleanup(self.n.close)

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as ctx:
            self.n.call(method, params, token)
        self.assertEqual(ctx.exception.code, code)

    def test_auth_agent_and_audit(self):
        n = self.n
        a, g = n.agent(ACTIONS, 1)
        did = g["delegation_ref"]["resource_id"]
        token_file = n.base / "agent.token"
        token_file.write_text(a["token"])
        token_file.chmod(0o600)
        self.denied("AuthenticationRequired", "Principal.WhoAmI", {}, "")
        self.denied("PermissionDenied", "Agent.Register", {"name": "escalate"}, a["token"])
        self.denied("PermissionDenied", "Task.Prepare",
                    dict(tool="volume.create", input={"name": "x"}, delegation_id=did, key=key()), a["token"])
        result = n.cli(token_file, "agent", "--key", "collect", "--delegation", did)
        self.assertEqual(result["task"]["lifecycle"], "Succeeded")
        self.assertIn("kernel_release", result["result"])
        self.assertEqual(n.get(result["task"]["current_run_id"])["resource"]["spec"]["agent_principal_id"], a["principal_id"])
        s = n.prepare("system.status", {}, key(), a["token"], did)
        self.denied("QuotaExceeded", "Task.Submit", s, a["token"])
        other, _ = n.agent(ACTIONS)
        self.denied("PermissionDenied", "Task.Get", {"id": result["task"]["task_id"]}, other["token"])
        self.denied("PermissionDenied", "Artifact.Read", {"reference": result["task"]["result"]["artifact_refs"][0]}, other["token"])
        n.call("Grant.Revoke", target(g["grant_ref"]))
        self.denied("PermissionDenied", "Task.Submit", s, a["token"])
        audit = json.dumps(n.call("Audit.List", {}))
        self.assertIn("PermissionDenied", audit)
        self.assertNotIn(a["token"], audit)
        self.assertNotIn(n.owner, audit)
        n.call("Principal.Revoke", target(n.get(a["principal_id"])["reference"]))
        self.denied("AuthenticationRequired", "Principal.WhoAmI", {}, a["token"])

    def test_schema_and_concurrent_idempotency(self):
        n = self.n
        s = n.prepare("system.status", {}, "stable")
        self.assertEqual(n.call("Task.LookupSubmission", s)["status"], "AbsentWithinWindow")
        self.denied("InvalidArgument", "Task.Submit", dict(s, task_id="client-forged"))
        self.denied("InvalidArgument", "Task.Submit", dict(s, request_digest="sha256:" + "0" * 64))
        with ThreadPoolExecutor(max_workers=12) as ex:
            receipts = list(ex.map(lambda _: n.call("Task.Submit", s), range(24)))
        self.assertEqual(len({r["task_id"] for r in receipts}), 1)
        self.assertEqual(sum(not r["deduplicated"] for r in receipts), 1)
        self.assertEqual(n.call("Task.LookupSubmission", s)["accepted"]["task_id"], receipts[0]["task_id"])
        changed = n.prepare("volume.create", {"name": "other"}, "stable")
        self.denied("IdempotencyConflict", "Task.Submit", changed)
        self.assertEqual(n.wait(receipts[0]["task_id"])["lifecycle"], "Succeeded")
        n.stop(signal.SIGKILL)
        n.start()
        self.assertTrue(n.call("Task.Submit", s)["deduplicated"])
        self.assertEqual(n.call("Task.Get", {"id": receipts[0]["task_id"]})["lifecycle"], "Succeeded")
        self.denied("UnsupportedCapability", "Task.Prepare", dict(tool="host.shell", key=key(), input={}, delegation_id="owner-delegation"))

    def test_sdk_lost_submit_ack_queries_original_namespace(self):
        import socket
        import struct
        n = self.n
        methods = []
        proxy = n.base / "ack-proxy.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(str(proxy))
            listener.listen()
            listener.settimeout(8)

            def forward():
                for _ in range(3):
                    stream, _ = listener.accept()
                    with stream, stream.makefile("rb") as f, socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as upstream:
                        size = f.read(4)
                        payload = f.read(struct.unpack(">I", size)[0])
                        method = json.loads(payload)["method"]
                        methods.append(method)
                        upstream.connect(str(n.socket))
                        upstream.sendall(size + payload)
                        with upstream.makefile("rb") as response:
                            size = response.read(4)
                            body = response.read(struct.unpack(">I", size)[0])
                        if method != "Task.Submit":
                            stream.sendall(size + body)

            with ThreadPoolExecutor(1) as executor:
                relay = executor.submit(forward)
                receipt = json.loads(subprocess.check_output([n.binaries / "psiosctl", "--socket", proxy,
                    "--token-file", n.witness / "owner.token", "submit", "system.status", "--key", "lost-ack"], input=b"{}", timeout=15))
                relay.result(timeout=8)
        self.assertEqual(methods, ["Task.Prepare", "Task.Submit", "Task.LookupSubmission"])
        self.assertTrue(receipt["deduplicated"])
        self.assertEqual(n.wait(receipt["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(len(n.call("Resource.List", {"kind": "Task"})["items"]), 1)

    def test_cancel_tombstone_retry_and_approval_cas(self):
        n = self.n
        s = n.prepare("system.status", {}, "cancel-first")
        cancel = {k: s[k] for k in ["schema_version", "origin_ref", "idempotency_key", "request_digest", "cancel_scope_id"]}
        request = dict(cancel, reason="aborted", cancel_request_id=key())
        blocked = n.call("Task.CancelOrigin", request)
        self.assertEqual(blocked["status"], "Blocked")
        self.assertEqual(n.call("Task.CancelOrigin", request), blocked)
        self.denied("IdempotencyConflict", "Task.CancelOrigin", dict(request, reason="changed"))
        self.denied("Cancelled", "Task.Submit", s)
        self.assertEqual(n.call("Task.LookupSubmission", s)["status"], "Expired")
        v = n.perform("volume.create", {"name": "work"})
        t = n.submit("volume.write", dict(**target(v), name="x", content="one"))
        waiting = n.wait(t)
        self.assertEqual(waiting["lifecycle"], "WaitingApproval")
        req = n.get(waiting["conditions"][0]["message"])
        params = dict(**target(req["reference"]), approve=True, plan_digest=req["value"]["plan_digest"])
        self.denied("ApprovalInvalidated", "Approval.Decide", dict(params, plan_digest="sha256:" + "1" * 64))
        n.call("Task.Cancel", dict(id=t, expected_resource_version=waiting["task_revision"]))
        self.denied("Conflict", "Approval.Decide", params)
        prior = n.get(t)["reference"]
        retry = n.call("Task.Retry", dict(**target(prior), key="try-again"))
        n.approve(retry["task_id"])
        self.assertEqual(n.wait(retry["task_id"])["retry_of_task_id"], t)
        uses = n.call("Resource.List", dict(kind="ApprovalUse"))["items"]
        self.assertEqual(len(uses), 1)
        self.assertEqual(n.call("Budget.Get", {"id": s["budget_ref"]["resource_id"]})["reserved"], 0)

    def test_competing_approved_writes(self):
        n = self.n
        v = n.perform("volume.create", {"name": "conflict"})
        jobs = [n.submit("volume.write", dict(**target(v), name="sample", content=value)) for value in ("a", "b")]
        with ThreadPoolExecutor(2) as pool:
            list(pool.map(n.approve, jobs))
        results = [n.wait(t) for t in jobs]
        self.assertEqual(sorted(t["lifecycle"] for t in results), ["Failed", "Succeeded"])
        self.assertEqual(next(t for t in results if t["lifecycle"] == "Failed")["outcome_knowledge"], "NotStarted")
        v = n.get(v["resource_id"])["reference"]
        self.assertIn(n.perform("volume.read", dict(**target(v), name="sample"))["content"], ["a", "b"])

    def test_list_watch_and_persistent_checkpoint(self):
        n = self.n
        page = n.call("Resource.List", {"kind": "Capability", "limit": 1})
        self.assertFalse(page["complete"])
        self.denied("CursorExpired", "Resource.List", dict(cursor=page["next_cursor"], kind="Volume"))
        page2 = n.call("Resource.List", {"cursor": page["next_cursor"], "limit": 2})
        self.assertEqual(page["snapshot_id"], page2["snapshot_id"])
        self.assertNotIn(page["items"][0], page2["items"])
        snapshot = n.call("Resource.List", {"kind": "Volume"})
        volume = n.perform("volume.create", {"name": "new"})
        cursor = snapshot["watch_start_cursor"]
        polled = n.call("Event.Poll", {"cursor": cursor})
        self.assertEqual(polled["events"][0]["resource_ref"], volume)
        self.assertEqual(n.call("Event.Poll", {"cursor": cursor})["events"], polled["events"])
        n.call("Event.Ack", dict(consumer="reader", cursor=polled["cursor"]))
        n.stop()
        n.start()
        self.assertEqual(n.call("Event.Offset", {"id": "reader"})["cursor"], polled["cursor"])
        self.denied("Conflict", "Event.Ack", dict(consumer="reader", cursor=cursor))
        self.assertEqual(n.call("Event.Poll", {"cursor": polled["cursor"]})["events"], [])
        n.agent(ACTIONS)
        self.denied("CursorExpired", "Event.Poll", {"cursor": cursor})

    def test_storage_backup_corruption_and_path_confinement(self):
        n = self.n
        v = n.perform("volume.create", {"name": "data"})
        v = n.perform("volume.write", dict(**target(v), name="record", content="original"))["volume_ref"]
        self.denied("InvalidArgument", "Task.Prepare", dict(tool="volume.read", delegation_id="owner-delegation",
                    input=dict(**target(v), name="../identity"), key=key()))
        b = n.perform("backup.create", target(v))
        v = n.perform("volume.write", dict(**target(v), name="record", content="changed"))["volume_ref"]
        clone = n.perform("backup.restore", target(b))
        self.assertNotEqual(clone["resource_id"], v["resource_id"])
        self.assertEqual(n.perform("volume.read", dict(**target(clone), name="record"))["content"], "original")
        self.assertEqual(n.perform("volume.read", dict(**target(v), name="record"))["content"], "changed")
        refused = n.wait(n.submit("volume.write", dict(**target(clone), name="record", content="before activation")), True)
        self.assertEqual(refused["conditions"][0]["type"], "PreconditionFailed")
        n.perform("volume.activate", target(clone))
        (n.state / "backups" / b["resource_id"] / "blob-0").write_text("tampered")
        failed = n.wait(n.submit("backup.restore", target(b)))
        self.assertEqual(failed["conditions"][0]["type"], "PreconditionFailed")
        files = n.state / "volumes" / v["resource_id"] / "data"
        (files / "binary").write_bytes(bytes([255]))
        binary_read = n.wait(n.submit("volume.read", dict(**target(v), name="binary")))
        self.assertEqual(binary_read["conditions"][0]["type"], "UnsupportedCapability")
        (files / "link").symlink_to(n.witness / "owner.token")
        refused = n.wait(n.submit("backup.create", target(v)))
        self.assertEqual(refused["conditions"][0]["type"], "UnsupportedCapability")
        self.assertEqual(n.wait(n.submit("volume.read", dict(**target(v), name="link")))["lifecycle"], "Failed")
        (files / "link").unlink()
        (files.parent / "identity").unlink()
        self.assertEqual(n.wait(n.submit("volume.read", dict(**target(v), name="record")))["lifecycle"], "Failed")
        self.assertFalse((files.parent / "identity").exists())

    def test_backup_retention_and_explicit_deletion(self):
        n = self.n
        v = n.perform("volume.create", {"name": "retained"})
        backup = n.perform("backup.create", target(v))
        refused = n.wait(n.submit("volume.delete", target(v)), autoapprove=True)
        self.assertEqual(refused["conditions"][0]["type"], "PreconditionFailed")
        gone = n.perform("backup.delete", target(backup))
        self.assertEqual(n.get(gone["resource_id"])["value"]["lifecycle"], "Deleted")
        n.perform("volume.delete", target(v))
        self.assertFalse((n.state / "volumes" / v["resource_id"]).exists())

    def test_secret_is_usable_without_disclosure(self):
        n = self.n
        value = "private-" + key()
        s = n.call("Secret.Put", dict(name="test", value=value))
        msg = "local challenge"
        result = n.perform("secret.mac", dict(secret_id=s["resource_id"], message=msg))
        self.assertEqual(base64.b64decode(result["tag"]), hmac.digest(value.encode(), msg.encode(), hashlib.sha256))
        self.assertNotIn(value, json.dumps(n.get(s["resource_id"])))
        n.call("Secret.Revoke", target(s))
        self.assertEqual(n.wait(n.submit("secret.mac", dict(secret_id=s["resource_id"], message=msg)))["lifecycle"], "Failed")
        n.stop()
        for path in n.state.glob("state.db*"):
            self.assertNotIn(value.encode(), path.read_bytes())

    def test_stale_database_quarantine_and_invalid_configuration(self):
        n = self.n
        a, grant = n.agent(ACTIONS)
        n.stop()
        old = n.base / "old.db"
        with closing(sqlite3.connect(n.state / "state.db")) as src, closing(sqlite3.connect(old)) as dst:
            src.backup(dst)
        n.start()
        n.call("Grant.Revoke", target(grant["grant_ref"]))
        n.stop()
        os.replace(old, n.state / "state.db")
        check = subprocess.run(n.daemon + ["check"], capture_output=True)
        self.assertNotEqual(check.returncode, 0)
        self.assertIn(b"RecoveryRequired", check.stderr)
        cfg = json.loads(n.config.read_text())
        cfg["queue_limit"] = 0
        n.config.write_text(json.dumps(cfg))
        check = subprocess.run(n.daemon + ["serve"], capture_output=True)
        self.assertIn(b"InvalidArgument", check.stderr)

    def test_delegation_attenuation_shared_budget_and_parent_revocation(self):
        n = self.n
        parent = n.call("Agent.Register", {"name": "coordinator"})
        child = n.call("Agent.Register", {"name": "worker", "peer_uid": os.geteuid()})
        grant = n.call("Grant.Issue", dict(principal_id=parent["principal_id"], actions=ACTIONS,
                                           units=1, ttl_seconds=90, allow_delegation=True))
        request = dict(parent_id=grant["delegation_ref"]["resource_id"], principal_id=child["principal_id"],
                       actions=ACTIONS, ttl_seconds=60)
        self.denied("PermissionDenied", "Delegation.Create", dict(request, actions=["volume.write"]), parent["token"])
        delegated = n.call("Delegation.Create", request, parent["token"])
        submit = n.prepare("system.status", {}, "child", child["token"], delegated["resource_id"])
        task = n.call("Task.Submit", submit, child["token"])
        self.assertEqual(n.wait(task["task_id"])["lifecycle"], "Succeeded")
        self.assertEqual(n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, parent["token"])["spent"], 1)
        self.denied("PermissionDenied", "Delegation.Create", dict(request, parent_id=delegated["resource_id"],
                    principal_id=parent["principal_id"]), child["token"])
        n.call("Delegation.Revoke", target(delegated), parent["token"])
        self.denied("PermissionDenied", "Task.Submit", submit, child["token"])

    def test_pool_admission_and_owner_recovery(self):
        n = self.n
        pool = n.call("StoragePool.Get", {"id": "local"})
        self.assertGreater(int(pool["available_bytes"]), 0)
        n.call("StoragePool.Configure", dict(**target(pool["pool_ref"]), allocation_enabled=False))
        t = n.wait(n.submit("volume.create", {"name": "blocked"}))
        self.assertEqual(t["conditions"][0]["type"], "PreconditionFailed")
        old = n.owner
        n.stop()
        subprocess.run(n.daemon + ["recover-owner"], check=True, stdout=subprocess.DEVNULL)
        n.owner = (n.witness / "owner.token").read_text()
        self.assertNotEqual(old, n.owner)
        n.start()
        self.denied("AuthenticationRequired", "Principal.WhoAmI", {}, old)
        self.assertEqual(n.call("Principal.WhoAmI", {})["principal_id"], "local-owner")

    def test_waiting_deadline_releases_budget(self):
        from datetime import datetime, timedelta, timezone
        n = self.n
        v = n.perform("volume.create", {"name": "deadline"})
        expires = (datetime.now(timezone.utc) + timedelta(seconds=2)).isoformat().replace("+00:00", "Z")
        req = n.call("Task.Prepare", dict(tool="volume.write", input=dict(**target(v), name="value", content="x"),
                     delegation_id="owner-delegation", key=key(), deadline_at=expires))
        t = n.call("Task.Submit", req)
        import time
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            observed = n.call("Task.Get", {"id": t["task_id"]})
            if observed["lifecycle"] == "Failed":
                break
            time.sleep(0.05)
        self.assertEqual(observed["conditions"][0]["type"], "DeadlineExceeded")
        self.assertEqual(n.call("Budget.Get", {"id": req["budget_ref"]["resource_id"]})["reserved"], 0)

    def test_application_engine_unavailable(self):
        n = self.n
        n.stop()
        cfg = json.loads(n.config.read_text())
        cfg["docker_socket"] = str(n.base / "missing-docker.sock")
        n.config.write_text(json.dumps(cfg))
        n.start()
        job = n.submit("application.install", dict(image_id="sha256:" + "a"*64, name="absent", release_version="1",
                     argv=["/test"], environment={}, license_expression="MIT", cpu_millicores=100, memory_max_bytes=33554432,
                     pids_max=32, ephemeral_max_bytes=65536))
        t = n.wait(job, True)
        self.assertEqual(t["lifecycle"], "Failed")
        self.assertEqual(t["conditions"][0]["type"], "DependencyUnavailable")
        self.assertEqual(n.call("Resource.List", {"kind": "Application"})["items"], [])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", default=BINARIES)
    args, other = parser.parse_known_args()
    BINARIES = args.bin_dir
    unittest.main(argv=["system.py", *other], verbosity=2)
