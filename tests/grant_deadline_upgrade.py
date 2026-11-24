"""Keep existing authority and require the new expiry-aware writer after migration."""
import argparse
import json
from pathlib import Path
import sqlite3
import subprocess

from support import Node


def exercise(previous, current):
    node = Node(previous)
    try:
        agent = node.call("Agent.Register", {"name": "legacy-clock"})
        issue = {"principal_id": agent["principal_id"], "actions": ["system.status"],
                 "tool_targets": ["profile-local"], "units": 1, "ttl_seconds": 180}
        legacy = node.call("Grant.Issue", issue)
        legacy_id = legacy["grant_ref"]["resource_id"]
        identity = node.call("Principal.WhoAmI", {})["authority_id"]
        node.stop()

        binary = current.resolve() / "psiosd"
        verify = [binary, "--config", node.config, "verify"]
        prior = json.loads(subprocess.check_output(verify, timeout=15))
        assert (prior["node_id"], prior["database_format"], prior["migration_required"]) == (identity, 24, True), prior
        node.daemon[0] = str(binary)
        node.start()
        assert node.get(legacy_id)["reference"] == legacy["grant_ref"]
        old_delegate = legacy["delegation_ref"]["resource_id"]
        old = node.call("Tool.Call", {"tool": "system.status", "input": {}, "delegation_id": old_delegate}, agent["token"])
        assert old["authority_id"] == identity
        new = node.call("Grant.Issue", issue)
        node.stop()

        observed = json.loads(subprocess.check_output(verify, timeout=15))
        assert observed["database_format"] >= 25 and not observed["migration_required"], observed
        with sqlite3.connect(f"file:{node.state / 'state.db'}?mode=ro", uri=True) as database:
            deadlines = dict(database.execute("SELECT resource,deadline_boot_ms FROM authorization_deadlines"))
        assert deadlines.keys() == {new["grant_ref"]["resource_id"], new["delegation_ref"]["resource_id"]}, deadlines
        assert len(set(deadlines.values())) == 1
        fenced = subprocess.run([previous.resolve() / "psiosd", "--config", node.config, "check"],
                                capture_output=True, text=True, timeout=15)
        assert fenced.returncode and "SchemaVersionUnsupported" in fenced.stderr, fenced
        print("PASS unchanged legacy grant, enrolled new deadlines and fenced the format 24 writer")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    exercise(args.previous_bin, args.current_bin)
