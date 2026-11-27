"""Carry format-124 chat history and receipts through the active-capacity migration."""

import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key
from test_chat import ACTIONS


def upgrade(previous, current):
    node = Node(previous)
    try:
        author, _ = node.agent(ACTIONS)
        reader, _ = node.agent(ACTIONS)
        author_token, reader_token = author["token"], reader["token"]

        def invitation(name):
            return {"key": key(), "name": name, "ttl_seconds": 600,
                    "recipient_principal_id": reader["principal_id"]}

        def respond(request, accept):
            opened = node.call("Conversation.OpenDirect", request, author_token)
            return node.call("Conversation.Respond", {
                "id": opened["id"], "expected_resource_version": opened["resource_version"], "accept": accept,
            }, reader_token)

        active = respond(invitation("current message history"), True)
        original = invitation("historical declined invitation")
        declined = respond(original, False)
        access = {"id": active["id"]}
        window = node.call("Message.OpenSendWindow", dict(access, ttl_seconds=600), author_token)
        message = node.call("Message.Prepare", {
            "conversation_id": active["id"], "client_send_id": f"{window['namespace']}:prior",
            "text": "original message and idempotency receipt",
        }, author_token)
        receipt = node.call("Message.Send", message, author_token)
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        command = [binary, "--config", node.config, "verify", "--scratch-dir", node.base]
        before = json.loads(subprocess.check_output(command, timeout=30))
        assert before["database_format"] == 124 and before["migration_required"], before
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Message.Send", message, author_token) == dict(receipt, deduplicated=True)
        assert node.call("Conversation.OpenDirect", original, author_token) == declined
        first_page = node.call("Conversation.List", {"limit": 1}, reader_token)
        second_page = node.call("Conversation.List", {"limit": 1, "after_id": first_page["next_after_id"]}, reader_token)
        assert {row["id"] for row in first_page["items"] + second_page["items"]} == {active["id"], declined["id"]}
        assert second_page["next_after_id"] is None, second_page
        assert respond(invitation("new active conversation"), True)["lifecycle"] == "Active"
        node.stop()

        after = json.loads(subprocess.check_output(command, timeout=30))
        assert after["database_format"] >= 125 and not after["migration_required"], after
        assert after["node_id"] == before["node_id"] and after["witness"] == "Confirmed", after
        old = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode and "SchemaVersionUnsupported" in old.stderr, old
        print("PASS format 124 chat history and original message receipt survived the indexed capacity migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    arguments = parser.parse_args()
    upgrade(arguments.previous_bin, arguments.current_bin)
