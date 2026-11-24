"""Add direct chat to an actual format-12 authority with an unfinished workflow."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Node, key
from test_chat import ACTIONS
from test_schedule import timestamp
from test_workflow import step
from workflow_upgrade import wait


def schedule_node(node, phase):
    record = node.call("Node.Get", {})["reference"]
    node.call("Node.Configure", dict(scheduling=phase, expected_resource_version=record["version"]))


def upgrade(previous, current):
    node = Node(previous)
    try:
        schedule_node(node, "Cordoned")
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "tool_targets": ["profile-local"], "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "system.status", "workflow.manage"], "units": 2,
        })
        definition = node.call("Workflow.Define", {"key": key(), "steps": [step("first"), step("last", after=["first"])]})
        request = {"key": key(), "definition_ref": definition["reference"], "deadline_at": timestamp(60),
                   "delegation_id": grant["delegation_ref"]["resource_id"]}
        workflow = node.call("Workflow.Start", request)
        assert workflow["state"]["lifecycle"] == "Ready", workflow
        node.stop(signal.SIGKILL)
        binary = current.resolve() / "psiosd"
        before = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert before["database_format"] == 12 and before["migration_required"], before
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Workflow.Start", request) == workflow
        alice, _ = node.agent(ACTIONS)
        bob, _ = node.agent(ACTIONS)
        pending = node.call("Conversation.OpenDirect", {
            "key": key(), "name": "upgrade note", "recipient_principal_id": bob["principal_id"], "ttl_seconds": 60,
        }, alice["token"])
        conversation = node.call("Conversation.Respond", dict(id=pending["id"], accept=True,
            expected_resource_version=pending["resource_version"]), bob["token"])
        window = node.call("Message.OpenSendWindow", dict(id=conversation["id"], ttl_seconds=60), alice["token"])
        message = node.call("Message.Prepare", dict(conversation_id=conversation["id"],
            client_send_id=f"{window['namespace']}:verified", text="Ready after upgrade"), alice["token"])
        accepted = node.call("Message.Send", message, alice["token"])
        schedule_node(node, "Enabled")
        result = wait(node, "Workflow.Get", workflow, {"Succeeded", "Failed", "Cancelled"})
        assert result["state"]["lifecycle"] == "Succeeded", result
        assert len({r["task_receipt"]["task_id"] for r in result["state"]["steps"]}) == 2
        node.stop(signal.SIGKILL)
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] >= 13 and not report["migration_required"], report
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode != 0 and "SchemaVersionUnsupported" in fenced.stderr, fenced
        node.start()
        history = node.call("Message.List", {"id": conversation["id"]}, bob["token"])
        entry, = history["items"]
        assert entry["message"]["message_id"] == accepted["message_id"], entry
        assert entry["content"]["parts"][0]["text"] == "Ready after upgrade"
        assert node.call("Message.Send", message, alice["token"]) == dict(accepted, deduplicated=True)
        assert node.call("Workflow.Start", request) == result
        print("PASS format 12 migration, preexisting workflow, private message recovery and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    opts = parser.parse_args()
    upgrade(opts.previous_bin, opts.current_bin)
