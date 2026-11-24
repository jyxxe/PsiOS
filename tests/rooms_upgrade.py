"""Migrate actual format-19 private messages, send namespaces and read positions."""
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
        sender, _ = node.agent(ACTIONS)
        reader, _ = node.agent(ACTIONS)
        send_token, read_token = sender["token"], reader["token"]
        pending = node.call("Conversation.OpenDirect", dict(
            key=key(), name="upgrade", ttl_seconds=600, recipient_principal_id=reader["principal_id"]), send_token)
        room = node.call("Conversation.Respond", dict(
            id=pending["id"], expected_resource_version=pending["resource_version"], accept=True), read_token)
        where = {"id": room["id"]}
        window = node.call("Message.OpenSendWindow", dict(where, ttl_seconds=600), send_token)

        def prepare(suffix, reply=None):
            return node.call("Message.Prepare", dict(
                conversation_id=room["id"], client_send_id=f"{window['namespace']}:{suffix}",
                text="durable conversation " + suffix, reply_to_message_id=reply), send_token)

        original = prepare("before")
        receipt = node.call("Message.Send", original, send_token)
        page = node.call("Message.List", where, read_token)
        ack = dict(where, cursor=page["cursor"])
        assert node.call("Message.AckRead", ack, read_token) == {"read_sequence": "1"}
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        before = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert before["database_format"] == 19 and before["migration_required"], before
        node.daemon[0] = str(binary)
        node.start()

        assert node.call("Message.Send", original, send_token) == dict(receipt, deduplicated=True)
        assert node.call("Message.AckRead", ack, read_token) == {"read_sequence": "1"}
        second = node.call("Message.Send", prepare("after", receipt["message_id"]), send_token)
        assert second["conversation_seq"] == "2" and second["writer_epoch"] == receipt["writer_epoch"], second
        resumed = node.call("Message.List", ack, read_token)
        message, = resumed["items"]
        assert message["message"]["reply_to_message_id"] == receipt["message_id"], message
        assert resumed["read_sequence"] == "1", resumed
        group = node.call("Conversation.OpenRoom", dict(key=key(), name="new-room", capacity=3, ttl_seconds=60), send_token)
        assert group["kind"] == "AdHocRoom"
        node.stop()
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] >= 20 and not report["migration_required"], report
        old = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                             capture_output=True, text=True, timeout=15)
        assert old.returncode != 0 and "SchemaVersionUnsupported" in old.stderr, old
        print("PASS format 19 migration, preserved acknowledged history, original window and cursor, fenced old writer")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    options = parser.parse_args()
    upgrade(options.previous_bin, options.current_bin)
