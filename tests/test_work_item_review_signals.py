"""Exercise exact review requests and replies through the native mailbox and Project API."""

from contextlib import closing
import hashlib
import json
import os
import signal
import socket
import sqlite3
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target


class WorkItemReviewSignalFixture:
    bin_dir = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")

    def setUp(self):
        self.n = Node(self.bin_dir)
        self.addCleanup(self.n.close)
        self.sender = self.n.call("Agent.Register", {"name": "review-contributor"})
        self.reviewer = self.n.call("Agent.Register", {"name": "independent-reviewer"})
        self.outside = self.n.call("Agent.Register", {"name": "mail-only-participant"})
        for actor in (self.sender, self.reviewer, self.outside):
            self.grant(actor, ["resource.read", "task.read", "event.read"])
        self.grant(self.sender, ["mail.send"])
        for recipient in (self.reviewer, self.outside):
            self.grant(recipient, ["mail.receive"])
        self.mailbox = self.call("Mailbox.Bind", {
            "name": "project-review", "sender_principal_id": self.sender["principal_id"],
        }, self.reviewer)["resource_id"]
        self.outside_mailbox = self.call("Mailbox.Bind", {
            "name": "outside-review", "sender_principal_id": self.sender["principal_id"],
        }, self.outside)["resource_id"]
        project = self.call("Project.Create", {"key": key(), "title": "Review real status evidence"})
        self.project = project["reference"]["resource_id"]
        self.grant({"principal_id": "local-owner"}, ["project.read", "project.manage"], [self.project])
        self.member(self.sender, "Contributor")
        self.member(self.reviewer, "Reviewer")
        self.grant(self.sender, ["project.read", "project.contribute"], [self.project])
        self.grant(self.reviewer, ["project.read", "project.review"], [self.project])
        profile = self.n.get("profile-local")["reference"]
        execution = self.grant(self.sender, ["task.submit", "task.read", "system.status"], [profile["resource_id"]])
        prepared = self.n.prepare("system.status", {}, token=self.sender["token"],
                                  delegation=execution["delegation_ref"]["resource_id"])
        task_id = self.call("Task.Submit", prepared, self.sender)["task_id"]
        task = self.n.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"))
        self.task = self.n.get(task_id)["reference"]
        self.artifact, = task["result"]["artifact_refs"]
        actual = self.call("Artifact.Read", {"reference": self.artifact}, self.sender)
        self.assertEqual(actual["authority_id"], self.call("Node.Get", {})["reference"]["resource_id"])
        project = self.call("Project.Get", {"id": self.project})
        item = self.call("WorkItem.Create", {
            "project_id": self.project, "expected_project_version": project["reference"]["version"],
            "key": key(), "title": "Check the observed node", "assignee_principal_id": self.sender["principal_id"],
            "criterion": {"kind": "TaskResult", "tool": "system.status", "target_ref": profile},
        })
        self.item = self.call("WorkItem.SubmitEvidence", {
            **target(item["reference"]), "task_ref": self.task, "artifact_ref": self.artifact,
        }, self.sender)["reference"]

    def call(self, method, params, actor=None):
        return self.n.call(method, params, actor["token"] if actor is not None else None)

    def grant(self, actor, actions, targets=None):
        params = {"principal_id": actor["principal_id"], "actions": actions, "units": 8, "ttl_seconds": 300}
        if targets is not None:
            params["tool_targets"] = targets
        return self.call("Grant.Issue", params)

    def result_grant(self):
        return self.grant(self.reviewer, ["task.results.read"], [self.task["resource_id"]])

    def member(self, actor, role):
        project = self.call("Project.Get", {"id": self.project})
        return self.call("Project.SetMember", {
            **target(project["reference"]), "principal_id": actor["principal_id"], "role": role,
        })

    def denied(self, code, method, params, actor=None):
        with self.assertRaises(Failure) as rejected:
            self.call(method, params, actor)
        self.assertEqual(rejected.exception.code, code, rejected.exception)

    def request(self, request_key, mailbox=None):
        return {"mailbox_id": mailbox or self.mailbox, "key": request_key,
                "topic": "ReviewRequested", "resource_ref": self.item}

    def reply(self, received):
        return {**target(self.item), "accept": True, "request_signal_ref": received}

    def inbox(self, **fields):
        return self.call("Mailbox.Read", {"id": self.mailbox, **fields}, self.reviewer)

    def receipt(self, ref, actor):
        return self.call("Signal.ReviewReceipt", {"signal_ref": ref}, actor)

    def visibility_cursors(self, kind, rid):
        return [
            (actor, self.call("Resource.List", {"kind": kind, "id": rid}, actor)["watch_start_cursor"])
            for actor in (None, self.reviewer, self.sender, self.outside)
        ]

    def assert_private(self, kind, reference, cursors):
        rid = reference["resource_id"]
        for actor, cursor in cursors:
            self.denied("PermissionDenied", "Resource.Get", {"id": rid}, actor)
            self.denied("PermissionDenied", "Resource.Resolve", {"reference": reference}, actor)
            self.denied("PermissionDenied", "Resource.Resolve", {
                "reference": {**reference, "version": "r99"},
            }, actor)
            listed = self.call("Resource.List", {"kind": kind, "id": rid}, actor)
            self.assertEqual(listed["items"], [])
            self.assertEqual(self.call("Event.Poll", {"cursor": cursor}, actor)["events"], [])

    def send_without_reply(self, params):
        request = json.dumps({"schema_version": "1.0.0", "request_id": key(),
                              "token": self.reviewer["token"], "method": "WorkItem.Review",
                              "params": params}).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as peer:
            peer.connect(str(self.n.socket))
            peer.sendall(struct.pack(">I", len(request)) + request)

    def verify(self):
        self.n.stop()
        result = json.loads(subprocess.check_output([*self.n.daemon, "verify"], timeout=20))
        self.assertGreaterEqual(result["database_format"], 110)
        self.assertFalse(result["migration_required"])


class WorkItemReviewSignals(WorkItemReviewSignalFixture, unittest.TestCase):
    def test_scoped_reply_is_atomic_and_exact_after_lost_response_and_restart(self):
        n = self.n
        request = self.request("exact-review")
        self.denied("PermissionDenied", "Signal.Send", request, self.sender)
        read_grant = self.result_grant()
        self.denied("PermissionDenied", "Signal.Send", self.request("foreign-review", self.outside_mailbox), self.sender)
        signal_input = json.dumps([self.mailbox, self.sender["principal_id"], request["key"]],
                                  separators=(",", ":")).encode()
        signal_id = "signal-" + hashlib.sha256(signal_input).hexdigest()
        signal_cursors = self.visibility_cursors("WorkItemReviewSignal", signal_id)
        receipt_cursors = self.visibility_cursors("WorkItemReviewReceipt", "work-item-review-receipt-" + signal_id)
        token = n.base / "sender.token"
        token.write_text(self.sender["token"])
        token.chmod(0o600)
        work = n.base / "item.json"
        work.write_text(json.dumps(self.item))
        sent = n.cli(token, "work-item", "request-review", self.mailbox, "--key", request["key"], str(work))
        self.assertEqual(self.call("Signal.Send", request, self.sender), sent)
        self.assertEqual(sent["resource_id"], signal_id)
        self.assert_private("WorkItemReviewSignal", sent, signal_cursors)
        msg, = self.inbox()["items"]
        self.assertEqual(msg["reference"], sent)
        self.assertEqual(msg["signal"]["resource_ref"], self.item)
        self.assertNotIn(self.artifact["resource_id"], json.dumps(msg))
        self.denied("PermissionDenied", "WorkItem.Review", self.reply(sent), self.outside)
        self.denied("PermissionDenied", "Signal.ReviewReceipt", {"signal_ref": sent}, self.outside)
        self.denied("NotFound", "Signal.ReviewReceipt", {"signal_ref": sent}, self.sender)

        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn, conn:
            conn.execute("""CREATE TRIGGER reject_review_reply BEFORE INSERT ON versions
                WHEN NEW.kind='WorkItemReviewReceipt'
                BEGIN SELECT RAISE(ABORT, 'injected review receipt failure'); END""")
        try:
            self.denied("StorageUnavailable", "WorkItem.Review", self.reply(sent), self.reviewer)
            self.assertEqual(self.call("WorkItem.Get", {"id": self.item["resource_id"]},
                                       self.reviewer)["reference"], self.item)
            self.denied("NotFound", "Signal.ReviewReceipt", {"signal_ref": sent}, self.sender)
        finally:
            with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn, conn:
                conn.execute("DROP TRIGGER reject_review_reply")
        self.call("Mailbox.Ack", {"id": self.mailbox, "previous": 0, "through": 1}, self.reviewer)
        self.assertEqual(self.inbox()["items"], [])
        self.send_without_reply(self.reply(sent))
        deadline = time.monotonic() + 10
        while True:
            try:
                committed = self.receipt(sent, self.sender)
                break
            except Failure as pending:
                self.assertEqual(pending.code, "NotFound", pending)
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.025)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.call("WorkItem.Review", self.reply(sent), self.reviewer), committed)
        self.denied("IdempotencyConflict", "WorkItem.Review", {**self.reply(sent), "accept": False}, self.reviewer)
        self.assertEqual(self.receipt(sent, self.sender), committed)
        self.assertEqual(committed["receipt"]["reviewed_item_ref"], self.item)
        self.assertTrue(committed["receipt"]["accepted"])
        self.assert_private("WorkItemReviewReceipt", committed["reference"], receipt_cursors)

        closed = self.call("WorkItem.Close", target(committed["receipt"]["work_item_ref"]))
        self.assertEqual(closed["value"]["lifecycle"], "Closed")
        self.call("Task.EvictResults", {**target(self.task), "reason": "private evidence was reviewed"})
        self.assertEqual(self.receipt(sent, self.sender), committed)
        self.call("Grant.Revoke", target(read_grant["grant_ref"]))
        self.denied("PermissionDenied", "Signal.ReviewReceipt", {"signal_ref": sent}, self.sender)
        self.denied("PermissionDenied", "WorkItem.Review", self.reply(sent), self.reviewer)
        n.stop()
        with closing(sqlite3.connect(n.state / "state.db", timeout=10)) as conn:
            identity = (committed["reference"]["resource_id"],)
            old, = conn.execute("SELECT body FROM versions WHERE id=? AND rev=1", identity).fetchone()
            with conn:
                conn.execute("UPDATE versions SET body=json_set(body,'$.accepted',json('false')) "
                             "WHERE id=? AND rev=1", identity)
            try:
                invalid = subprocess.run(n.daemon + ["verify"], capture_output=True, timeout=20)
                self.assertNotEqual(invalid.returncode, 0, invalid.stdout)
                self.assertIn(b"review reply does not attest", invalid.stderr)
            finally:
                with conn:
                    conn.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (old, *identity))
        self.verify()

    def test_removed_reviewer_cannot_restore_old_request_by_rejoining(self):
        read_grant = self.result_grant()
        sent = self.call("Signal.Send", self.request("before-removal"), self.sender)
        plain = self.call("Signal.Send", {"mailbox_id":self.mailbox,"key":"plain","topic":"ObservationReady"}, self.sender)
        expected = {"reference":None, "signal":{"seq":1,"availability":"Restricted"}}
        self.call("Grant.Revoke", target(read_grant["grant_ref"]))
        self.assertEqual(self.inbox()["items"][0], expected)
        self.denied("PermissionDenied", "WorkItem.Review", self.reply(sent), self.reviewer)
        self.assertEqual(self.call("WorkItem.Get", {"id": self.item["resource_id"]},
                                   self.reviewer)["reference"], self.item)
        self.result_grant()
        self.assertEqual(self.inbox()["items"][0]["reference"], sent)
        self.member(self.reviewer, None)
        first, second = self.inbox()["items"]
        self.assertEqual(first, expected)
        self.assertEqual(second["reference"], plain)
        self.member(self.reviewer, "Reviewer")
        self.assertEqual(self.inbox()["items"][0], expected)
        self.denied("PermissionDenied", "WorkItem.Review", self.reply(sent), self.reviewer)
        self.denied("PermissionDenied", "Signal.Send", self.request("before-removal"), self.sender)
        self.call("Mailbox.Ack", {"id": self.mailbox, "previous": 0, "through": 2}, self.reviewer)
        fresh = self.call("Signal.Send", self.request("after-rejoin"), self.sender)
        msg, = self.inbox()["items"]
        self.assertEqual((msg["reference"], msg["signal"]["seq"]), (fresh, 3))
        result = self.call("WorkItem.Review", self.reply(fresh), self.reviewer)
        self.assertEqual(self.receipt(fresh, self.sender), result)
        self.verify()
