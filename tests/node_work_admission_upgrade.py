"""Upgrade a real format-92 node holding a result, an approval and a cordon."""
import argparse
from pathlib import Path
import signal
import subprocess

from project_upgrade import verify
from service_journal import refuses
from support import Node, target


def configure(node, mode):
    ref = node.call("Node.Get", {})["reference"]
    return node.call("Node.Configure", {"expected_resource_version": ref["version"], "scheduling": mode})


def set_admission(node, view, mode):
    return node.call("Node.WorkAdmission.Set", {
        "expected_resource_version": view["reference"]["version"],
        "mode": mode,
        "reason": "exercise pre-upgrade Task admission",
    })


def upgrade(previous, current):
    node = Node(previous)
    previous_daemon = node.daemon.copy()
    try:
        completed = node.wait(node.submit("system.status", {}))
        assert completed["lifecycle"] == "Succeeded", completed
        artifact, = completed["result"]["artifact_refs"]
        payload = node.call("Artifact.Read", {"reference": artifact})

        volume = node.perform("volume.create", {"name": "approval-from-format-92"})
        written = node.state / "volumes" / volume["resource_id"] / "data" / "preserved"
        cordoned = configure(node, "Cordoned")
        request = node.prepare("volume.write", {
            **target(volume), "name": written.name, "content": "approved after format 93 migration",
        })
        accepted = node.call("Task.Submit", request)
        waiting = node.wait(accepted["task_id"])
        assert waiting["lifecycle"] == "WaitingApproval", waiting
        fresh = node.prepare("system.status", {})
        refuses("UnsupportedCapability", lambda: node.call("Node.WorkAdmission.Get", {}))

        node.stop(signal.SIGKILL)
        old = verify(node)
        assert old["database_format"] == 92 and not old["migration_required"], old
        witness = (node.witness / "head").read_bytes()
        node.binaries = current.resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        staged = verify(node)
        assert staged["database_format"] == 92 and staged["migration_required"], staged
        assert staged["node_id"] == old["node_id"]
        assert (node.witness / "head").read_bytes() == witness

        node.start()
        view = node.call("Node.WorkAdmission.Get", {})
        assert view["admission"]["mode"] == "Open" and view["task_counts"]["phases"]["WaitingApproval"] == 1, view
        state = node.call("Node.Get", {})
        assert state["reference"] == cordoned and state["state"]["scheduling"] == "Cordoned", state
        assert node.call("Task.Get", {"id": accepted["task_id"]}) == waiting
        assert node.call("Artifact.Read", {"reference": artifact}) == payload
        draining = set_admission(node, view, "Draining")
        replay = node.call("Task.Submit", request)
        assert replay["deduplicated"] and replay["task_id"] == accepted["task_id"], replay
        refuses("PreconditionFailed", lambda: node.call("Task.Submit", fresh))

        node.approve(accepted["task_id"])
        configure(node, "Enabled")
        result = node.wait(accepted["task_id"])
        assert result["lifecycle"] == "Succeeded", result
        output, = result["result"]["artifact_refs"]
        written_result = node.call("Artifact.Read", {"reference": output})
        assert written_result["volume_ref"] == node.get(volume["resource_id"])["reference"], written_result
        assert written.read_text() == "approved after format 93 migration"
        opened = set_admission(node, draining, "Open")
        assert opened["reference"]["resource_id"] == draining["reference"]["resource_id"]
        assert not opened["task_counts"]["phases"], opened
        fresh_task = node.call("Task.Submit", fresh)["task_id"]
        assert node.wait(fresh_task)["lifecycle"] == "Succeeded"

        node.stop()
        report = verify(node)
        assert report["database_format"] >= 93 and not report["migration_required"], report
        assert report["node_id"] == old["node_id"] and report["witness"] == "Confirmed", report
        obsolete = subprocess.run([*previous_daemon, "check"], capture_output=True, text=True, timeout=15)
        assert obsolete.returncode and "SchemaVersionUnsupported" in obsolete.stderr, obsolete
        print("PASS format-92 result, cordon and live approval survive format-93 work admission migration")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
