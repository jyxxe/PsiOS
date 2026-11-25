"""Upgrade an actual PR #1 state and witness in place, including accepted but unexecuted work."""
import argparse
import json
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Failure, Node, target


def main(previous):
    n = Node(previous)
    try:
        owner = n.owner
        authority = n.call("Principal.WhoAmI", {})["authority_id"]
        a, g = n.agent(["task.read", "task.submit", "system.status"])
        credential = a["token"]
        d = g["delegation_ref"]["resource_id"]
        s = n.prepare("system.status", {}, "accepted-before-upgrade", credential, d)
        v = n.perform("volume.create", dict(name="old-volume"))
        v = n.perform("volume.write", dict(**target(v), name="old-data", content="retained"))["volume_ref"]
        legacy_backup = n.perform("backup.create", target(v))
        legacy_secret = n.call("Secret.Put", dict(name="previous-secret", value="old-key"))
        n.stop()
        base = json.loads(n.config.read_text())
        n.config.write_text(json.dumps(dict(base, tick_ms=5000)))
        n.start()
        receipt = n.call("Task.Submit", s, credential)
        assert n.call("Task.Get", dict(id=receipt["task_id"]))["lifecycle"] == "Queued"
        w = n.submit("volume.write", dict(**target(v), name="old-data", content="new"))
        n.stop(signal.SIGKILL)
        binary = str(Path("target/debug/psiosd").absolute())
        report = json.loads(subprocess.check_output([binary, "--config", n.config, "verify"]))
        assert report["database_format"] == 1 and report["migration_required"], report
        assert report["node_id"] == authority
        with sqlite3.connect(n.state / "state.db") as db:
            assert db.execute("PRAGMA user_version").fetchone() == (1,)
        n.daemon[0] = binary
        n.config.write_text(json.dumps(base))
        n.start()
        assert n.call("Principal.WhoAmI", {})["authority_id"] == authority
        import base64, hmac
        mac_params = dict(secret_id=legacy_secret["resource_id"], message="upgrade")
        assert base64.b64decode(n.perform("secret.mac", mac_params)["tag"]) == hmac.digest(b"old-key", b"upgrade", "sha256")
        n.call("Secret.Rotate", dict(**target(legacy_secret), value="new-key"))
        assert base64.b64decode(n.perform("secret.mac", mac_params)["tag"]) == hmac.digest(b"new-key", b"upgrade", "sha256")
        assert n.owner == owner == (n.witness / "owner.token").read_text()
        assert n.wait(receipt["task_id"])["lifecycle"] == "Succeeded"
        assert n.call("Task.LookupSubmission", s, credential)["accepted"]["task_id"] == receipt["task_id"]
        assert n.call("Task.Submit", s, credential)["deduplicated"]
        assert n.perform("volume.read", dict(**target(v), name="old-data"))["content"] == "retained"
        grant = n.call("Grant.Issue", dict(principal_id="local-owner", actions=["task.submit", "task.read", "backup.verify"],
                       units=2, ttl_seconds=120, tool_targets=[legacy_backup["resource_id"]]))
        verify = n.prepare("backup.verify", target(legacy_backup), delegation=grant["delegation_ref"]["resource_id"])
        checked = n.wait(n.call("Task.Submit", verify)["task_id"])
        assert checked["lifecycle"] == "Succeeded", checked
        evidence = n.call("Artifact.Read", {"reference": checked["result"]["artifact_refs"][0]})
        assert evidence["bytes"] == str(len("retained")) and evidence["backup_ref"] == legacy_backup
        n.approve(w)
        assert n.wait(w)["lifecycle"] == "Succeeded"
        try:
            n.prepare("node.inventory", {}, "ungranted-new-tool", credential, d)
        except Failure as e:
            assert e.code == "PermissionDenied", e
        else:
            raise AssertionError("migration widened an existing agent grant")
        assert n.call("Budget.Get", {"id": g["budget_ref"]["resource_id"]}, credential)["model_calls_maximum"] == 0
        n.stop()
        with sqlite3.connect(n.state / "state.db") as db:
            assert db.execute("PRAGMA user_version").fetchone() == (43,)
        old = subprocess.run([str(Path(previous) / "psiosd"), "--config", n.config, "serve"], capture_output=True)
        assert old.returncode != 0 and b"SchemaVersionUnsupported" in old.stderr
        n.start()
        assert n.perform("node.inventory", {})["boot_id"]
        print("PASS upgrade from PR #1 binary, credentials, queue, approval, data, dedup, restricted scope and downgrade refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
