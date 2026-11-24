"""Exercise local room membership and its shared durable message log."""
from concurrent.futures import ThreadPoolExecutor
import signal
import unittest

from support import Failure, Node, key, target
from test_schedule import after


ACTIONS = ["chat.invite", "chat.read", "chat.send"]


class RoomTests(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)
        self.alice, self.alice_grant = self.node.agent(ACTIONS)
        self.bob, self.bob_grant = self.node.agent(ACTIONS)
        self.carol, _ = self.node.agent(ACTIONS)
        self.a, self.b, self.c = [agent["token"] for agent in [self.alice, self.bob, self.carol]]

    def rejected(self, code, method, params, token):
        with self.assertRaises(Failure) as problem:
            self.node.call(method, params, token)
        self.assertEqual(problem.exception.code, code, problem.exception)

    def version(self, room):
        return dict(id=room["id"], expected_resource_version=room["resource_version"])

    def create(self, capacity=3, ttl_seconds=120):
        return self.node.call("Conversation.OpenRoom", dict(
            key=key(), name="operations", capacity=capacity, ttl_seconds=ttl_seconds), self.a)

    def invite(self, room, principal, ttl_seconds=60):
        return self.node.call("Conversation.Invite", dict(
            self.version(room), principal_id=principal["principal_id"], ttl_seconds=ttl_seconds), self.a)

    def get(self, room, token):
        return self.node.call("Conversation.Get", {"id": room["id"]}, token)

    def join(self, room, principal):
        invitation = self.invite(room, principal)
        return self.node.call("Conversation.Respond", dict(
            self.version(invitation["conversation"]), membership_id=invitation["membership_id"],
            accept=True), principal["token"])

    def page(self, room, token, **options):
        return self.node.call("Message.List", dict(id=room["id"], **options), token)

    def window(self, room, token):
        return self.node.call("Message.OpenSendWindow", dict(id=room["id"], ttl_seconds=60), token)

    def prepare(self, room, token, text, window=None, reply=None):
        window = window or self.window(room, token)
        request = dict(conversation_id=room["id"], client_send_id=f"{window['namespace']}:{key()}",
                       text=text, reply_to_message_id=reply)
        return self.node.call("Message.Prepare", request, token)

    def send(self, room, token, text, reply=None):
        return self.node.call("Message.Send", self.prepare(room, token, text, reply=reply), token)

    def test_room_consent_capacity_and_fixed_direct_membership(self):
        n = self.node
        spec = dict(key=key(), name="pair-and-group", capacity=3, ttl_seconds=120)
        room = n.call("Conversation.OpenRoom", spec, self.a)
        self.assertEqual(n.call("Conversation.OpenRoom", spec, self.a), room)
        self.rejected("IdempotencyConflict", "Conversation.OpenRoom", dict(spec, capacity=4), self.a)
        self.rejected("PermissionDenied", "Conversation.Get", {"id": room["id"]}, self.c)
        self.assertEqual(n.call("Conversation.List", {}, self.c)["items"], [])
        self.assertEqual(room["kind"], "AdHocRoom")
        invited = self.invite(room, self.bob)
        invited_room = invited["conversation"]
        view = self.get(room, self.b)
        self.assertEqual([m["principal_id"] for m in view["members"]], [self.bob["principal_id"]])
        self.rejected("PermissionDenied", "Message.List", {"id": room["id"]}, self.b)
        self.rejected("PermissionDenied", "Conversation.Respond", dict(
            self.version(invited_room), membership_id=invited["membership_id"], accept=True), self.a)

        decline = n.call("Conversation.Respond", dict(
            self.version(invited_room), membership_id=invited["membership_id"], accept=False), self.b)
        self.assertEqual(decline["members"][0]["reason"], "InvitationDeclined")
        self.assertEqual(n.call("Conversation.List", {}, self.b)["items"], [])
        room = self.join(self.get(room, self.a), self.bob)
        self.assertNotEqual(room["membership_id"], invited["membership_id"])
        self.rejected("PermissionDenied", "Conversation.Invite", dict(
            self.version(room), principal_id=self.carol["principal_id"], ttl_seconds=60), self.b)
        room = self.join(room, self.carol)
        self.assertEqual({m["principal_id"] for m in room["members"]},
                         {agent["principal_id"] for agent in [self.alice, self.bob, self.carol]})
        another, _ = n.agent(ACTIONS)
        self.rejected("QueueFull", "Conversation.Invite", dict(
            self.version(room), principal_id=another["principal_id"], ttl_seconds=60), self.a)
        n.call("Principal.Revoke", target(n.get(self.carol["principal_id"])["reference"]))
        room = self.get(room, self.a)
        self.assertNotIn(self.carol["principal_id"], {m["principal_id"] for m in room["members"]})
        self.invite(room, another)

        direct = n.call("Conversation.OpenDirect", dict(
            key=key(), name="private", recipient_principal_id=self.bob["principal_id"], ttl_seconds=60), self.a)
        self.rejected("UnsupportedCapability", "Conversation.Invite", dict(
            self.version(direct), principal_id=self.carol["principal_id"], ttl_seconds=60), self.a)
        self.rejected("PermissionDenied", "Conversation.Get", {"id": direct["id"]}, another["token"])

    def test_expired_invitation_releases_capacity_and_room_expiry_fences_writes(self):
        room = self.create(capacity=2)
        old = self.invite(room, self.bob, ttl_seconds=1)
        invitation = self.get(room, self.b)["members"][0]
        after(invitation["invitation_valid_until"])
        expired = self.get(room, self.b)["members"][0]
        self.assertEqual((expired["lifecycle"], expired["reason"]), ("Expired", "InvitationExpired"))
        self.rejected("PreconditionFailed", "Conversation.Respond", dict(
            self.version(old["conversation"]), membership_id=old["membership_id"], accept=True), self.b)
        self.join(old["conversation"], self.carol)
        self.rejected("PermissionDenied", "Conversation.Get", {"id": room["id"]}, self.b)

        short = self.create(ttl_seconds=1)
        after(short["expires_at"])
        self.assertEqual(self.get(short, self.a)["lifecycle"], "Archived")
        self.rejected("PreconditionFailed", "Message.OpenSendWindow", dict(id=short["id"], ttl_seconds=60), self.a)

    def test_history_begins_at_join_and_hidden_reply_is_filtered(self):
        n = self.node
        room = self.create()
        hidden = self.send(room, self.a, "owner-only before the join")
        room = self.join(room, self.bob)
        page = self.page(room, self.b)
        self.assertEqual((page["items"], page["read_sequence"]), ([], "0"))
        self.assertEqual(n.call("Message.AckRead", dict(id=room["id"], cursor=page["cursor"]), self.b),
                         {"read_sequence": "0"})
        lookup = dict(conversation_id=room["id"], message_id=hidden["message_id"])
        self.rejected("NotFound", "Message.Get", lookup, self.b)
        with self.assertRaises(Failure) as problem:
            self.prepare(room, self.b, "cannot reply across the join boundary", reply=hidden["message_id"])
        self.assertEqual(problem.exception.code, "NotFound")

        visible = self.send(room, self.a, "visible response", reply=hidden["message_id"])
        pending = self.prepare(room, self.b, "durable reply", reply=visible["message_id"])
        receipt = n.call("Message.Send", pending, self.b)
        n.stop(signal.SIGKILL)
        n.start()
        duplicate = n.call("Message.Send", pending, self.b)
        self.assertEqual(duplicate, dict(receipt, deduplicated=True))
        page = self.page(room, self.b)
        self.assertEqual([row["message"]["message_id"] for row in page["items"]],
                         [visible["message_id"], receipt["message_id"]])
        self.assertNotIn("reply_to_message_id", page["items"][0]["message"])
        got = n.call("Message.Get", dict(lookup, message_id=visible["message_id"]), self.b)
        self.assertEqual(got, page["items"][0])
        self.assertEqual(page["items"][1]["message"]["reply_to_message_id"], visible["message_id"])
        owner = self.page(room, self.a)
        self.assertEqual(owner["items"][1]["message"]["reply_to_message_id"], hidden["message_id"])

    def test_removal_fences_concurrent_send_and_rejoin_creates_new_identity(self):
        n = self.node
        room = self.join(self.create(), self.bob)
        self.send(room, self.b, "before removal")
        before = self.page(room, self.b)
        self.assertEqual(n.call("Message.AckRead", dict(id=room["id"], cursor=before["cursor"]), self.b),
                         {"read_sequence": "1"})
        pending = self.prepare(room, self.b, "concurrent with removal")
        remove = dict(self.version(room), membership_id=room["membership_id"])

        def publish():
            try:
                return n.call("Message.Send", pending, self.b)["message_id"]
            except Failure as problem:
                self.assertEqual(problem.code, "PermissionDenied")
                return None

        with ThreadPoolExecutor(max_workers=2) as workers:
            sent = workers.submit(publish)
            changed = workers.submit(n.call, "Conversation.RemoveMember", remove, self.a)
            message_id, room = sent.result(), changed.result()
        owner_messages = [row["message"]["message_id"] for row in self.page(room, self.a)["items"]]
        self.assertEqual(len(owner_messages), 2 if message_id else 1)
        if message_id:
            self.assertEqual(owner_messages[-1], message_id)
        self.rejected("PermissionDenied", "Message.Send", pending, self.b)
        self.rejected("PermissionDenied", "Message.List", {"id": room["id"], "cursor": before["cursor"]}, self.b)

        room = self.join(room, self.bob)
        self.assertNotEqual(room["membership_id"], remove["membership_id"])
        self.rejected("CursorExpired", "Message.List", {"id": room["id"], "cursor": before["cursor"]}, self.b)
        self.rejected("IdempotencyKeyExpired", "Message.Send", pending, self.b)
        self.assertEqual(self.page(room, self.b)["read_sequence"], "0")
        self.assertEqual(self.page(room, self.b)["items"], [])
        self.send(room, self.b, "member again")
        self.rejected("PreconditionFailed", "Conversation.Leave", self.version(room), self.a)
        left = n.call("Conversation.Leave", self.version(room), self.b)
        self.assertEqual(left["membership"]["reason"], "LeftByMember")
        self.rejected("PermissionDenied", "Message.List", {"id": room["id"]}, self.b)

    def test_revocation_and_target_scopes_survive_recovery(self):
        n = self.node
        room = self.join(self.create(), self.bob)
        original = self.send(room, self.a, "retained when the owner is revoked")
        n.call("Grant.Revoke", target(self.bob_grant["grant_ref"]))
        self.rejected("PermissionDenied", "Message.List", {"id": room["id"]}, self.b)
        node_id = n.call("Node.Get", {})["reference"]["resource_id"]
        wrong = n.call("Grant.Issue", dict(principal_id=self.bob["principal_id"], actions=ACTIONS,
                                          ttl_seconds=400, units=1, tool_targets=[node_id]))
        self.assertEqual(n.call("Conversation.List", {}, self.b)["items"], [])
        self.rejected("PermissionDenied", "Conversation.Get", {"id": room["id"]}, self.b)
        n.call("Grant.Revoke", target(wrong["grant_ref"]))
        n.call("Grant.Issue", dict(principal_id=self.bob["principal_id"], actions=ACTIONS,
                                  ttl_seconds=400, units=1))
        n.call("Principal.Revoke", target(n.get(self.alice["principal_id"])["reference"]))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(self.get(room, self.b)["lifecycle"], "Archived")
        self.assertEqual(self.page(room, self.b)["items"][0]["message"]["message_id"], original["message_id"])
        self.rejected("PreconditionFailed", "Message.OpenSendWindow", dict(id=room["id"], ttl_seconds=60), self.b)
        self.rejected("AuthenticationRequired", "Conversation.Get", {"id": room["id"]}, self.a)
