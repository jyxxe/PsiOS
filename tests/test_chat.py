"""Use real local principals to exercise durable private message delivery and reads."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import signal
import socket
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from test_schedule import after


ACTIONS = ["chat.invite", "chat.read", "chat.send"]


class DirectChatTests(unittest.TestCase):
    def setUp(self):
        self.node = Node()
        self.addCleanup(self.node.close)
        self.alice, self.alice_grant = self.node.agent(ACTIONS)
        self.bob, self.bob_grant = self.node.agent(ACTIONS)
        self.eve, _ = self.node.agent([*ACTIONS, "resource.read", "event.read", "task.read"])
        self.a = self.alice["token"]
        self.b = self.bob["token"]
        self.e = self.eve["token"]

    def rejected(self, code, method, params, token=None):
        with self.assertRaises(Failure) as problem:
            self.node.call(method, params, token)
        self.assertEqual(problem.exception.code, code, problem.exception)

    def invitation(self, name="handoff", ttl_seconds=120):
        return dict(key=key(), name=name, recipient_principal_id=self.bob["principal_id"], ttl_seconds=ttl_seconds)

    def respond(self, room, accept=True, token=None):
        return self.node.call("Conversation.Respond", dict(id=room["id"], accept=accept,
            expected_resource_version=room["resource_version"]), self.b if token is None else token)

    def ready(self, name="handoff"):
        return self.respond(self.node.call("Conversation.OpenDirect", self.invitation(name), self.a))

    def window(self, room, token, ttl_seconds=120):
        return self.node.call("Message.OpenSendWindow", dict(id=room["id"], ttl_seconds=ttl_seconds), token)

    def message(self, room, window, suffix, text, token, reply=None):
        request = {"conversation_id": room["id"], "client_send_id": f"{window['namespace']}:{suffix}", "text": text}
        if reply:
            request["reply_to_message_id"] = reply
        return self.node.call("Message.Prepare", request, token)

    def page(self, room, token, **options):
        return self.node.call("Message.List", dict(id=room["id"], **options), token)

    def send_without_reading_reply(self, message):
        data = json.dumps(dict(schema_version="1.0.0", token=self.a, request_id=key(),
                               method="Message.Send", params=message)).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.connect(str(self.node.socket))
            stream.sendall(struct.pack(">I", len(data)) + data)

    def test_invitation_consent_named_direct_conversations_and_privacy(self):
        n = self.node
        request = self.invitation()
        room = n.call("Conversation.OpenDirect", request, self.a)
        self.assertEqual(n.call("Conversation.OpenDirect", request, self.a), room)
        self.rejected("IdempotencyConflict", "Conversation.OpenDirect", dict(request, name="different"), self.a)
        self.assertEqual(room["members"][1]["lifecycle"], "Invited")
        self.assertEqual(n.call("Conversation.List", {}, self.b)["items"], [room])
        self.assertEqual(n.call("Conversation.List", {}, self.e)["items"], [])
        self.rejected("PermissionDenied", "Message.OpenSendWindow", {"id": room["id"], "ttl_seconds": 60}, self.a)
        self.rejected("PermissionDenied", "Conversation.Respond", dict(
            id=room["id"], accept=True, expected_resource_version=room["resource_version"]), self.a)
        self.rejected("PermissionDenied", "Conversation.Get", {"id": room["id"]}, self.e)
        joined = self.respond(room)
        self.assertEqual(joined["membership_revision"], "2")
        self.assertEqual(joined["members"][1]["membership_id"], room["members"][1]["membership_id"])
        self.assertEqual(joined["members"][1]["lifecycle"], "Active")
        self.rejected("Conflict", "Conversation.Respond", dict(id=room["id"], accept=True,
                      expected_resource_version=room["resource_version"]), self.b)
        self.assertNotEqual(self.ready("same peers").get("id"), joined["id"])
        n.call("Grant.Issue", {"principal_id": "local-owner", "actions": ["chat.read"], "units": 1, "ttl_seconds": 60})
        self.rejected("PermissionDenied", "Conversation.Get", {"id": room["id"]})
        self.assertEqual(n.call("Resource.List", {"kind": "Conversation"})["items"], [])

    def test_declined_and_expired_invitations_release_slots_and_preserve_paged_history(self):
        n = self.node
        pending = [n.call("Conversation.OpenDirect", self.invitation(f"work-{index}"), self.a)
                   for index in range(64)]
        self.rejected("QueueFull", "Conversation.OpenDirect", self.invitation(), self.a)
        declined = self.respond(pending[0], accept=False)
        expiring = n.call("Conversation.OpenDirect", self.invitation("short invitation", 1), self.a)
        after(expiring["invitation_valid_until"])
        n.stop(signal.SIGKILL)
        n.start()

        def open_invitation(request):
            try:
                return n.call("Conversation.OpenDirect", request, self.a)
            except Failure as error:
                self.assertEqual(error.code, "QueueFull", error)
                return None

        with ThreadPoolExecutor(max_workers=2) as callers:
            admissions = list(callers.map(open_invitation, [self.invitation(), self.invitation()]))
        admitted, = [item for item in admissions if item is not None]
        self.assertEqual(n.call("Conversation.Get", {"id": declined["id"]}, self.b), declined)
        self.assertEqual(n.call("Conversation.Get", {"id": expiring["id"]}, self.b)["members"][1]["lifecycle"], "Expired")

        expected = sorted([item["id"] for item in [*pending, expiring, admitted]])
        received = []
        params = {"limit": 16}
        for offset in range(0, len(expected), params["limit"]):
            page = n.call("Conversation.List", params, self.b)
            ids = [item["id"] for item in page["items"]]
            end = offset + params["limit"]
            self.assertEqual(ids, expected[offset:end])
            self.assertEqual(page["next_after_id"], ids[-1] if end < len(expected) else None)
            received.extend(ids)
            params["after_id"] = page["next_after_id"]
        self.assertEqual(received, expected)
        self.assertEqual(n.call("Conversation.List", {"limit": 16}, self.e), {"items": [], "next_after_id": None})
        self.rejected("InvalidArgument", "Conversation.List", {"limit": 65}, self.b)

    def test_lost_ack_and_process_crash_keep_one_sequence_and_authenticated_sender(self):
        n = self.node
        room = self.ready()
        a_window = self.window(room, self.a)
        self.assertEqual(self.window(room, self.a), a_window)
        b_window = self.window(room, self.b)
        msg = self.message(room, a_window, "stable", "已完成\n保留回执", self.a)
        self.rejected("IdempotencyKeyExpired", "Message.Send", msg, self.b)
        self.rejected("InvalidArgument", "Message.Send", dict(msg, sender_principal_id=self.bob["principal_id"]), self.a)
        tampered = dict(msg, content={"parts": [{"kind": "text", "text": "unexpected"}]})
        self.rejected("InvalidArgument", "Message.Send", tampered, self.a)
        self.send_without_reading_reply(msg)
        end = time.monotonic() + 5
        while not (items := self.page(room, self.b)["items"]):
            self.assertLess(time.monotonic(), end)
            time.sleep(0.05)
        original, = items
        unrelated = n.call("Grant.Issue", dict(principal_id=self.alice["principal_id"], actions=["system.status"],
                           tool_targets=["profile-local"], units=1, ttl_seconds=60))
        n.call("Grant.Revoke", target(unrelated["grant_ref"]))
        self.assertEqual(self.window(room, self.a), a_window)
        self.assertEqual(self.window(room, self.b), b_window)
        n.stop(signal.SIGKILL)
        n.start()
        with ThreadPoolExecutor(max_workers=4) as threads:
            copies = list(threads.map(lambda _: n.call("Message.Send", msg, self.a), range(4)))
        self.assertEqual(copies, [copies[0]] * 4)
        receipt = copies[0]
        self.assertTrue(receipt["deduplicated"])
        self.assertEqual((receipt["message_id"], receipt["conversation_seq"], receipt["writer_epoch"]),
                         (original["message"]["message_id"], "1", "1"))
        self.assertEqual(original["message"]["sender_principal_id"], self.alice["principal_id"])
        self.assertEqual(original["message"]["initial_send_digest"], msg["content_digest"])
        self.assertEqual(original["content"]["parts"][0]["text"], "已完成\n保留回执")
        altered = self.message(room, a_window, "stable", "changed", self.a)
        self.rejected("IdempotencyConflict", "Message.Send", altered, self.a)
        reply = self.message(room, b_window, "reply", "收到", self.b, reply=receipt["message_id"])
        answer = n.call("Message.Send", reply, self.b)
        view = n.call("Message.Get", dict(conversation_id=room["id"], message_id=answer["message_id"]), self.a)
        self.assertEqual(view["message"]["reply_to_message_id"], receipt["message_id"])
        pinned = dict(conversation_id=room["id"], message_id=answer["message_id"],
                      content_ref=view["message"]["current_content_ref"])
        self.assertEqual(n.call("Message.Get", pinned, self.a), view)
        self.rejected("PreconditionFailed", "Message.Get",
                      dict(pinned, content_ref=dict(pinned["content_ref"], version="r100")), self.a)
        self.assertEqual(answer["conversation_seq"], "2")
        self.rejected("PermissionDenied", "Message.Get", dict(conversation_id=room["id"], message_id=answer["message_id"]), self.e)
        self.assertEqual(n.call("Resource.List", {"kind": "Message"})["items"], [])
        self.rejected("NotFound", "Resource.Get", {"id": answer["message_id"]})

    def test_owner_credential_recovery_and_peer_disable_close_send_windows(self):
        n = self.node
        n.call("Grant.Issue", dict(principal_id="local-owner", actions=ACTIONS, units=1, ttl_seconds=90))
        invitation = n.call("Conversation.OpenDirect", self.invitation())
        room = self.respond(invitation)
        accepted = self.message(room, self.window(room, self.b), "accepted", "keep this message", self.b)
        first = n.call("Message.Send", accepted, self.b)
        original_owner = n.owner
        n.stop()
        subprocess.run([*n.daemon, "recover-owner"], check=True, capture_output=True)
        n.owner = (n.witness / "owner.token").read_text()
        self.assertNotEqual(n.owner, original_owner)
        n.start()
        self.rejected("IdempotencyKeyExpired", "Message.Send", accepted, self.b)
        self.assertEqual(self.page(room, None)["items"][0]["message"]["message_id"], first["message_id"])
        pending = self.message(room, self.window(room, None), "pending", "permission will change", None)
        n.call("Principal.Revoke", target(n.get(self.bob["principal_id"])["reference"]))
        self.rejected("PermissionDenied", "Message.Send", pending)
        self.assertEqual(len(self.page(room, None)["items"]), 1)

    def test_actor_and_conversation_bound_cursors_and_read_acknowledgments(self):
        n = self.node
        room = self.ready()
        other = self.ready("independent history")
        window = self.window(room, self.a)
        for ordinal in range(3):
            n.call("Message.Send", self.message(room, window, str(ordinal), f"text {ordinal}", self.a), self.a)
        first = self.page(room, self.b, limit=1)
        self.rejected("InvalidArgument", "Message.List", {"id": room["id"], "limit": 33}, self.b)
        self.assertTrue(first["has_more"])
        self.assertEqual(first["read_sequence"], "0")
        self.assertEqual(first["items"][0]["message"]["conversation_seq"], "1")
        self.rejected("CursorExpired", "Message.List", {"id": room["id"], "cursor": first["cursor"]}, self.a)
        self.rejected("CursorExpired", "Message.List", {"id": other["id"], "cursor": first["cursor"]}, self.b)
        forged = ("A" if first["cursor"][0] != "A" else "B") + first["cursor"][1:]
        self.rejected("CursorExpired", "Message.AckRead", {"id": room["id"], "cursor": forged}, self.b)
        final = self.page(room, self.b, limit=2, cursor=first["cursor"])
        self.assertEqual([r["message"]["conversation_seq"] for r in final["items"]], ["2", "3"])
        self.assertFalse(final["has_more"])
        self.assertEqual(final["read_sequence"], "0")
        ack = {"id": room["id"], "cursor": final["cursor"]}
        self.assertEqual(n.call("Message.AckRead", ack, self.b), {"read_sequence": "3"})
        self.rejected("Conflict", "Message.AckRead", {"id": room["id"], "cursor": first["cursor"]}, self.b)
        without_access = {"conversation_id": other["id"], "message_id": first["items"][0]["message"]["message_id"]}
        self.rejected("NotFound", "Message.Get", without_access, self.b)
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Message.AckRead", ack, self.b), {"read_sequence": "3"})
        self.assertEqual(self.page(room, self.a)["read_sequence"], "0")

    def test_expired_send_namespace_revoked_member_and_archival(self):
        n = self.node
        invitation = n.call("Conversation.OpenDirect", self.invitation("expiring", 1), self.a)
        after(invitation["invitation_valid_until"])
        expired = n.call("Conversation.Get", {"id": invitation["id"]}, self.b)
        self.assertEqual(expired["members"][1]["lifecycle"], "Expired")
        self.rejected("PreconditionFailed", "Conversation.Respond", dict(id=invitation["id"],
                      expected_resource_version=invitation["resource_version"], accept=True), self.b)
        declined = self.respond(n.call("Conversation.OpenDirect", self.invitation("declined"), self.a), accept=False)
        self.assertEqual(declined["members"][1]["reason"], "InvitationDeclined")
        room = self.ready()
        window = self.window(room, self.a, ttl_seconds=3)
        msg = self.message(room, window, "once", "fixed", self.a)
        first = n.call("Message.Send", msg, self.a)
        cursor = self.page(room, self.a)["cursor"]
        after(window["valid_until"])
        self.rejected("IdempotencyKeyExpired", "Message.Send", msg, self.a)
        fresh = self.window(room, self.a)
        self.assertNotEqual(fresh["namespace"], window["namespace"])
        more = self.message(room, fresh, "once", "new window", self.a)
        n.call("Grant.Revoke", target(self.bob_grant["grant_ref"]))
        self.rejected("PermissionDenied", "Message.Send", more, self.a)
        self.rejected("PermissionDenied", "Message.List", {"id": room["id"]}, self.b)
        self.rejected("CursorExpired", "Message.List", {"id": room["id"], "cursor": cursor}, self.a)
        n.call("Grant.Issue", dict(principal_id=self.bob["principal_id"], actions=ACTIONS, units=10, ttl_seconds=60))
        self.rejected("IdempotencyKeyExpired", "Message.Send", more, self.a)
        self.rejected("PermissionDenied", "Conversation.Archive", dict(id=room["id"],
                      expected_resource_version=room["resource_version"]), self.b)
        archived = n.call("Conversation.Archive", dict(id=room["id"],
                         expected_resource_version=room["resource_version"]), self.a)
        self.assertEqual(archived["lifecycle"], "Archived")
        self.assertEqual(self.page(room, self.b)["items"][0]["message"]["message_id"], first["message_id"])
        self.rejected("PreconditionFailed", "Message.OpenSendWindow", dict(id=room["id"], ttl_seconds=60), self.a)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION"), "requires a bounded filesystem")
    def test_low_disk_keeps_private_history_and_refuses_new_sends(self):
        n = self.node
        n.stop()
        n.config.write_text(json.dumps(dict(json.loads(n.config.read_text()),
                                            authority_reserve={"state_bytes": 4 * 1024 * 1024})))
        n.start()
        room = self.ready()
        window = self.window(room, self.a)
        original = self.message(room, window, "saved", "durable", self.a)
        accepted = n.call("Message.Send", original, self.a)
        pending = self.message(room, window, "pending", "wait for space", self.a)
        storage = n.call("Node.Get", {})["authority_storage"]["state"]
        self.assertLessEqual(int(storage["capacity_bytes"]), 64 * 1024 * 1024)
        fill = n.base / "fill-chat"
        with fill.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, int(storage["available_bytes"]) - 2 * 1024 * 1024)
        self.addCleanup(fill.unlink, missing_ok=True)
        self.assertTrue(n.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        self.rejected("QuotaExceeded", "Message.Send", pending, self.a)
        self.assertEqual(n.call("Message.Send", original, self.a), dict(accepted, deduplicated=True))
        history = self.page(room, self.b)
        self.assertEqual(len(history["items"]), 1)
        self.assertEqual(n.call("Message.AckRead", dict(id=room["id"], cursor=history["cursor"]), self.b),
                         {"read_sequence": "1"})
        fill.unlink()
        self.assertEqual(n.call("Message.Send", pending, self.a)["conversation_seq"], "2")
