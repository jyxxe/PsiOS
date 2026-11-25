"""Upgrade an actual PR #1 state and witness in place, including accepted but unexecuted work."""
import argparse
from contextlib import closing
import hashlib
import json
from pathlib import Path
import signal
import sqlite3
import subprocess

from support import Failure, Node, target


def authority_contents(node):
    files = (file for root in (node.state, node.witness) for file in root.rglob("*") if file.is_file())
    return {file: (file.stat().st_mtime_ns, hashlib.sha256(file.read_bytes()).digest()) for file in files}


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
        original = authority_contents(n)
        scratch = n.base / "rehearsal"
        scratch.mkdir(mode=0o700)
        rehearsed = json.loads(subprocess.check_output([binary, "--config", n.config, "verify",
            "--rehearse-migration", "--scratch-dir", scratch], timeout=20))
        migration = rehearsed.pop("migration_rehearsal")
        assert rehearsed == report and migration["status"] == "Verified", rehearsed
        assert migration["source_format"] == 1 and migration["target_format"] > 1, migration
        assert authority_contents(n) == original and not list(scratch.iterdir())
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            assert db.execute("PRAGMA user_version").fetchone() == (1,)
            db.execute("CREATE INDEX versions_origin_seq ON versions(seq)")
        # An integral, witnessed database can still be incompatible with a new DDL step.
        conflicting = authority_contents(n)
        clean = json.loads(subprocess.check_output([binary, "--config", n.config, "verify"], timeout=20))
        assert clean["database_format"] == 1 and clean["sqlite_integrity"] == "Verified", clean
        failed = subprocess.run([binary, "--config", n.config, "verify", "--rehearse-migration",
            "--scratch-dir", scratch], capture_output=True, timeout=20)
        assert failed.returncode != 0 and not failed.stdout, failed
        assert b"migration rehearsal failed" in failed.stderr and b"StorageUnavailable" in failed.stderr, failed
        assert authority_contents(n) == conflicting and not list(scratch.iterdir())
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DROP INDEX versions_origin_seq")
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
        upgraded = json.loads(subprocess.check_output(n.daemon + ["verify"], timeout=15))
        assert upgraded["database_format"] == migration["target_format"], upgraded
        assert not upgraded["migration_required"] and upgraded["node_id"] == authority, upgraded
        with sqlite3.connect(n.state / "state.db") as db:
            assert db.execute("PRAGMA user_version").fetchone() == (upgraded["database_format"],)
        old = subprocess.run([str(Path(previous) / "psiosd"), "--config", n.config, "serve"], capture_output=True)
        assert old.returncode != 0 and b"SchemaVersionUnsupported" in old.stderr
        n.start()
        assert n.perform("node.inventory", {})["boot_id"]
        print("PASS private migration rehearsal and upgrade from PR #1 binary with credentials, queue, data and downgrade refusal")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin", required=True)
    main(parser.parse_args().previous_bin)
