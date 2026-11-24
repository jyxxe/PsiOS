"""Upgrade an actual format-13 authority with pending timers and previously sent chat."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from chat_upgrade import schedule_node
from support import Node, key
from test_chat import ACTIONS
from test_schedule import timestamp
from workflow_upgrade import wait


def upgrade(previous, current):
    node = Node(previous)
    try:
        schedule_node(node, "Cordoned")
        grant = node.call("Grant.Issue", {
            "principal_id": "local-owner", "tool_targets": ["profile-local"], "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "system.status", "schedule.manage"], "units": 3,
        })
        request = {"tool": "system.status", "input": {}, "delegation_id": grant["delegation_ref"]["resource_id"],
                   "run_at": timestamp(2), "deadline_at": timestamp(60)}
        once_request = dict(request, key=key())
        once = node.call("Schedule.Create", once_request)
        repeat_request = dict(request, key=key(), repeat={"count": 2, "interval_seconds": 1,
            "overlap": "Allow", "missed": {"mode": "BoundedCatchUp", "limit": 2}})
        interval = node.call("Schedule.Create", repeat_request)

        alice, _ = node.agent(ACTIONS)
        bob, _ = node.agent(ACTIONS)
        invitation = node.call("Conversation.OpenDirect", {
            "key": key(), "name": "release note", "recipient_principal_id": bob["principal_id"], "ttl_seconds": 90,
        }, alice["token"])
        conversation = node.call("Conversation.Respond", dict(id=invitation["id"], accept=True,
            expected_resource_version=invitation["resource_version"]), bob["token"])
        window = node.call("Message.OpenSendWindow", dict(id=conversation["id"], ttl_seconds=90), alice["token"])
        prepared = node.call("Message.Prepare", dict(conversation_id=conversation["id"],
            client_send_id=f"{window['namespace']}:release", text="Calendar upgrade"), alice["token"])
        accepted = node.call("Message.Send", prepared, alice["token"])
        node.stop(signal.SIGKILL)

        binary = current.resolve() / "psiosd"
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] == 13 and report["migration_required"], report
        original = node.daemon[0]
        node.daemon[0] = str(binary)
        node.start()
        assert node.call("Schedule.Create", once_request) == once
        assert node.call("Schedule.Create", repeat_request) == interval
        assert node.call("Message.Send", prepared, alice["token"]) == dict(accepted, deduplicated=True)
        received, = node.call("Message.List", {"id": conversation["id"]}, bob["token"])["items"]
        assert received["message"]["message_id"] == accepted["message_id"], received
        schedule_node(node, "Enabled")

        settled_once = wait(node, "Schedule.Get", once, {"Triggered", "Expired", "Rejected"})["state"]
        assert settled_once["lifecycle"] == "Triggered" and settled_once["trigger_id"] == once["state"]["trigger_id"]
        task_ids = {settled_once["task_receipt"]["task_id"]}
        settled_interval = wait(node, "Schedule.Get", interval, {"Completed", "Rejected"})["state"]
        assert settled_interval["lifecycle"] == "Completed", settled_interval
        first, second = settled_interval["trigger_refs"]
        assert first["resource_id"] == interval["state"]["next"]["trigger_id"]
        for trigger in (first, second):
            state = node.call("Schedule.TriggerGet", {"id": trigger["resource_id"]})["state"]
            assert state["lifecycle"] == "Triggered", state
            task_ids.add(state["task_receipt"]["task_id"])
        assert len(task_ids) == 3 and all(node.wait(task)["lifecycle"] == "Succeeded" for task in task_ids)

        node.stop()
        report = json.loads(subprocess.check_output([binary, "--config", node.config, "verify"]))
        assert report["database_format"] >= 14 and not report["migration_required"], report
        fenced = subprocess.run([original, "--config", node.config, "check"], capture_output=True, text=True, timeout=15)
        assert fenced.returncode != 0 and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS format 13 migration, original one-shot and interval receipts, private chat and old-writer refusal")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", default=Path("target/debug"), type=Path)
    opts = parser.parse_args()
    upgrade(opts.previous_bin, opts.current_bin)
