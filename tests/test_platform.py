"""Real procfs/sysfs, event consumers, concurrent submissions and multi-chunk file lifecycles."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import signal
import time
import unittest

from support import Failure, Node, key, target


class PlatformTests(unittest.TestCase):
    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)

    def denied(self, code, method, params, token=None):
        with self.assertRaises(Failure) as e:
            self.n.call(method, params, token)
        self.assertEqual(e.exception.code, code)

    def test_proc_sysfs_and_permission_scope(self):
        n = self.n
        info = n.perform("node.inventory", {})
        self.assertEqual(info["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        self.assertGreater(info["memory_bytes"]["MemTotal"], 0)
        ps = n.perform("node.processes", {"after_pid": os.getpid() - 1, "limit": 1})
        self.assertEqual(ps["items"][0]["pid"], os.getpid())
        self.assertEqual(ps["items"][0]["start_ticks"], Path("/proc/self/stat").read_text().rsplit(") ", 1)[1].split()[19])
        dev = n.perform("node.devices", {})["devices"]
        self.assertEqual({d["name"] for d in dev}, {p.name for p in Path("/sys/class/block").iterdir()})
        a = n.call("Agent.Register", {"name": "scoped"})
        g = n.call("Grant.Issue", dict(principal_id=a["principal_id"], units=2, ttl_seconds=90,
            actions=["task.submit", "task.read", "node.inventory", "system.status"], tool_targets=[info_id(n)]))
        d = g["delegation_ref"]["resource_id"]
        wrong = n.call("Agent.Register", {"name": "wrong-node-scope"})
        n.call("Grant.Issue", dict(principal_id=wrong["principal_id"], units=1, ttl_seconds=90, actions=["node.inventory"],
            tool_targets=["profile-local"]))
        self.denied("PermissionDenied", "Node.Get", {}, wrong["token"])
        req = n.prepare("system.status", {}, key(), a["token"], d)
        self.denied("PermissionDenied", "Task.Submit", req, a["token"])
        self.denied("PermissionDenied", "Tool.Call", dict(tool="system.status", input={}, delegation_id=d), a["token"])
        req = n.prepare("node.inventory", {}, key(), a["token"], d)
        self.assertEqual(n.wait(n.call("Task.Submit", req, a["token"])["task_id"])["lifecycle"], "Succeeded")
        self.denied("PermissionDenied", "Node.Configure", dict(scheduling="Cordoned", expected_resource_version="r1"), a["token"])

    def test_cordon_resumes_durable_queue_history_archive_and_dedup(self):
        n = self.n
        node = n.call("Node.Get", {})
        n.call("Node.Configure", dict(expected_resource_version=node["reference"]["version"], scheduling="Cordoned"))
        self.denied("Conflict", "Node.Configure", dict(expected_resource_version=node["reference"]["version"], scheduling="Enabled"))
        requests = [n.prepare("system.status", {}, str(i)) for i in range(12)]
        with ThreadPoolExecutor(8) as pool:
            receipts = list(pool.map(lambda req: n.call("Task.Submit", req), requests))
        self.assertEqual(n.call("Node.Get", {})["queue"]["Queued"], 12)
        n.stop(signal.SIGKILL)
        n.start()
        node = n.call("Node.Get", {})
        self.assertEqual(node["state"]["scheduling"], "Cordoned")
        n.call("Node.Configure", dict(expected_resource_version=node["reference"]["version"], scheduling="Enabled"))
        for receipt in receipts:
            self.assertEqual(n.wait(receipt["task_id"])["lifecycle"], "Succeeded")
        tid = receipts[0]["task_id"]
        page = n.call("Task.History", dict(id=tid, limit=2))
        version = page["through_version"]
        transitions = list(page["items"])
        while page["next_after_version"]:
            page = n.call("Task.History", dict(id=tid, limit=2, through_version=version, after_version=page["next_after_version"]))
            transitions.extend(page["items"])
        self.assertEqual(transitions[0]["lifecycle"], "Queued")
        self.assertEqual(transitions[-1]["lifecycle"], "Succeeded")
        self.assertIn("InFlight", [v["outcome_knowledge"] for v in transitions])
        archive = n.call("Task.Archive", dict(id=tid, expected_resource_version=version))
        self.assertEqual(n.call("Task.Archive", dict(id=tid, expected_resource_version=version)), archive)
        self.assertEqual(n.call("Node.Get", {})["queue"]["Done"], 11)
        self.assertEqual(n.call("Task.LookupSubmission", requests[0])["accepted"]["task_id"], tid)
        self.assertTrue(n.call("Task.Submit", requests[0])["deduplicated"])
        self.assertEqual(n.call("Artifact.Read", dict(reference=n.wait(tid)["result"]["artifact_refs"][0]))["authority_id"], info_id(n))

    def test_event_wait_wakes_times_out_and_revocation_wakes_consumer(self):
        n = self.n
        agent, grant = n.agent(["resource.read", "task.read", "event.read"])
        token = agent["token"]
        cursor = n.call("Resource.List", {"kind": "Node"}, token)["watch_start_cursor"]
        with ThreadPoolExecutor(1) as pool:
            listener = pool.submit(n.call, "Event.Wait", dict(cursor=cursor, timeout_ms=5000), token)
            time.sleep(.15)
            node = n.call("Node.Get", {})
            changed = n.call("Node.Configure", dict(scheduling="Cordoned", expected_resource_version=node["reference"]["version"]))
            out = listener.result(3)
            self.assertEqual(out["events"][0]["resource_ref"], changed)
            before = time.monotonic()
            self.assertEqual(n.call("Event.Wait", dict(cursor=out["cursor"], timeout_ms=150), token)["events"], [])
            self.assertGreaterEqual(time.monotonic() - before, .12)
            listener = pool.submit(n.call, "Event.Wait", dict(cursor=out["cursor"], timeout_ms=5000), token)
            time.sleep(.1)
            n.call("Grant.Revoke", target(grant["grant_ref"]))
            with self.assertRaises(Failure) as error:
                listener.result(2)
            self.assertEqual(error.exception.code, "PermissionDenied")

    def test_nested_binary_files_paging_checksum_backup_and_stale_offset(self):
        n = self.n
        v = n.perform("volume.create", {"name": "binary"})
        v = n.perform("volume.mkdir", dict(target=target(v), path="dataset"))["volume_ref"]
        v = n.perform("volume.mkdir", dict(target=target(v), path="dataset/parts"))["volume_ref"]
        data = bytes(range(256)) * 935
        name = "dataset/parts/binary"
        for pos in range(0, len(data), 64 * 1024):
            v = n.perform("volume.append", dict(target=target(v), path=name, offset=pos,
                data_base64=base64.b64encode(data[pos:pos + 65536]).decode()))["volume_ref"]
        stale = n.wait(n.submit("volume.append", dict(target=target(v), path=name, offset=1, data_base64="YQ==")), True)
        self.assertEqual(stale["conditions"][0]["type"], "Conflict")
        v = n.perform("volume.mkdir", dict(target=target(v), path="dataset/more"))["volume_ref"]
        first = n.perform("volume.files", dict(target=target(v), path="dataset", limit=1))
        later = n.perform("volume.files", dict(target=target(v), path="dataset", after=first["next_after"]))
        self.assertEqual([r["name"] for r in first["entries"] + later["entries"]], ["more", "parts"])
        backup = n.perform("backup.create", target(v))
        restored = n.perform("backup.restore", target(backup))
        out = bytearray()
        while len(out) < len(data):
            part = n.perform("volume.read_chunk", dict(target=target(restored), path=name, offset=len(out), limit=90000))
            out.extend(base64.b64decode(part["data_base64"], validate=True))
        self.assertTrue(part["eof"])
        self.assertEqual(out, data)
        self.denied("InvalidArgument", "Task.Prepare", dict(tool="volume.read_chunk", key=key(), delegation_id="owner-delegation",
            input=dict(target=target(v), path="dataset/../escape", offset=0, limit=10)))
        root = n.state / "volumes" / v["resource_id"] / "data"
        (root / "escape").symlink_to(n.witness)
        refused = n.wait(n.submit("volume.read_chunk", dict(target=target(v), path="escape/owner.token", offset=0, limit=100)))
        self.assertEqual(refused["lifecycle"], "Failed")
        v = n.perform("volume.remove_entry", dict(target=target(v), path=name))["volume_ref"]
        self.assertFalse((root / name).exists())
        self.assertEqual(n.perform("volume.files", dict(target=target(v), path="dataset/parts"))["entries"], [])

    def test_secret_rotation_fences_old_plan_and_persists_only_ciphertext(self):
        import hmac
        n = self.n
        old, new = key(), key()
        secret = n.call("Secret.Put", dict(name="credential", value=old))
        node = n.call("Node.Get", {})
        cordoned = n.call("Node.Configure", dict(expected_resource_version=node["reference"]["version"], scheduling="Cordoned"))
        params = dict(secret_id=secret["resource_id"], message="challenge")
        before = n.submit("secret.mac", params)
        rotated = n.call("Secret.Rotate", dict(**target(secret), value=new))
        self.denied("Conflict", "Secret.Rotate", dict(**target(secret), value=old))
        n.stop(signal.SIGKILL)
        n.start()
        n.call("Node.Configure", dict(expected_resource_version=cordoned["version"], scheduling="Enabled"))
        lost = n.wait(before)
        self.assertEqual(lost["conditions"][0]["type"], "ApprovalInvalidated")
        result = n.perform("secret.mac", params)
        self.assertEqual(base64.b64decode(result["tag"]), hmac.digest(new.encode(), b"challenge", "sha256"))
        revoked = n.call("Secret.Revoke", target(rotated))
        self.denied("PreconditionFailed", "Secret.Rotate", dict(**target(revoked), value=old))
        n.stop()
        for f in n.state.glob("state.db*"):
            self.assertNotIn(old.encode(), f.read_bytes())
            self.assertNotIn(new.encode(), f.read_bytes())

    def test_multihop_delegation_narrows_targets_and_cuts_queued_work(self):
        n = self.n
        root, middle, leaf = [n.call("Agent.Register", {"name": name}) for name in ("root", "middle", "leaf")]
        actions = ["task.submit", "task.read", "node.inventory", "system.status"]
        node_id = info_id(n)
        grant = n.call("Grant.Issue", dict(principal_id=root["principal_id"], units=3, ttl_seconds=120, actions=actions,
            allow_delegation=True, max_delegation_depth=2, tool_targets=[node_id, "profile-local"]))
        parent = n.call("Delegation.Create", dict(parent_id=grant["delegation_ref"]["resource_id"], principal_id=middle["principal_id"],
            actions=actions, ttl_seconds=90, allow_delegation=True, tool_targets=[node_id]), root["token"])
        child = dict(parent_id=parent["resource_id"], principal_id=leaf["principal_id"], actions=actions, ttl_seconds=60)
        self.denied("PermissionDenied", "Delegation.Create", dict(child, tool_targets=["profile-local"]), middle["token"])
        self.denied("PermissionDenied", "Delegation.Create", dict(child, allow_delegation=True), middle["token"])
        self.denied("PermissionDenied", "Delegation.Create", dict(child, principal_id=root["principal_id"]), middle["token"])
        leaf_d = n.call("Delegation.Create", child, middle["token"])
        did = leaf_d["resource_id"]
        denied = n.prepare("system.status", {}, "outside", leaf["token"], did)
        self.denied("PermissionDenied", "Task.Submit", denied, leaf["token"])
        req = n.prepare("node.inventory", {}, key(), leaf["token"], did)
        executed = n.call("Task.Submit", req, leaf["token"])
        self.assertEqual(n.wait(executed["task_id"])["lifecycle"], "Succeeded")
        state = n.call("Node.Get", {})
        n.call("Node.Configure", dict(expected_resource_version=state["reference"]["version"], scheduling="Cordoned"))
        req = n.prepare("node.inventory", {}, key(), leaf["token"], did)
        pending = n.call("Task.Submit", req, leaf["token"])
        n.call("Delegation.Revoke", target(grant["delegation_ref"]))
        failed = n.wait(pending["task_id"])
        self.assertEqual((failed["outcome_knowledge"], failed["conditions"][0]["type"]), ("NotStarted", "PermissionDenied"))
        budget = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((budget["reserved"], budget["spent"]), (0, 1))
        n.stop()
        n.start()
        self.denied("PermissionDenied", "Task.Submit", req, leaf["token"])

    def test_consumer_scope_and_position_survive_authorization_epoch_change(self):
        n = self.n
        prev = n.call("Resource.List", {"kind": "Node"})["watch_start_cursor"]
        n.call("Event.Ack", dict(consumer="node-observer", cursor=prev))
        n.agent(["task.read"])
        wrong = n.call("Resource.List", {"kind": "Task"})["watch_start_cursor"]
        self.denied("Conflict", "Event.Ack", dict(consumer="node-observer", previous=prev, cursor=wrong))
        fresh = n.call("Resource.List", {"kind": "Node"})["watch_start_cursor"]
        ack = n.call("Event.Ack", dict(consumer="node-observer", previous=prev, cursor=fresh))
        self.assertEqual(ack["cursor"], fresh)

    def test_rejection_and_superseded_single_use_commit_consistently(self):
        n = self.n
        v = n.perform("volume.create", {"name": "approval"})
        opts = dict(**target(v), name="never", content="denied")
        tid = n.submit("volume.write", opts)
        decision = n.approve(tid, approve=False)
        self.assertEqual(decision["decision"], "Rejected")
        terminal = n.wait(tid)
        self.assertEqual((terminal["lifecycle"], terminal["outcome_knowledge"]), ("Cancelled", "NotStarted"))
        self.assertEqual(n.call("Resource.List", {"kind": "ApprovalUse"})["items"], [])
        cordon = n.call("Node.Get", {})
        n.call("Node.Configure", dict(expected_resource_version=cordon["reference"]["version"], scheduling="Cordoned"))
        another = n.submit("volume.write", opts)
        approved = n.approve(another)
        queued = n.call("Task.Get", {"id": another})
        n.call("Task.Cancel", dict(id=another, expected_resource_version=queued["task_revision"]))
        unit = n.get("use-" + approved["decision_id"])["value"]
        self.assertEqual((unit["binding_status"], unit["validity_status"]), ("Unbound", "Superseded"))
        self.assertFalse((n.state / "volumes" / v["resource_id"] / "data" / "never").exists())

    def test_bad_worker_and_service_config(self):
        n = self.n
        n.stop()
        base = json.loads(n.config.read_text())
        import subprocess
        for update in (dict(workers=0), dict(workers=17), dict(services=[dict(unit="-bad.service", manager="user")]),
                       dict(services=[dict(unit="*.service", manager="system")])):
            n.config.write_text(json.dumps(dict(base, **update)))
            cmd = subprocess.run(n.daemon + ["check"], capture_output=True)
            self.assertNotEqual(cmd.returncode, 0)
            self.assertIn(b"InvalidArgument", cmd.stderr)


def info_id(node):
    return node.call("Principal.WhoAmI", {})["authority_id"]


if __name__ == "__main__":
    unittest.main(verbosity=2)
