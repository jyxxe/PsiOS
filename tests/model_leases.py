"""Exercise boot-bound model leases using the real local provider API."""
import argparse
import json
from pathlib import Path
import signal
import subprocess

from support import Failure, Node


def open_session(call, key, seconds=300):
    skill = call("Skill.Publish", dict(name="clock-observer", version="1", tools=["system.status"],
                source="local-test", license="0BSD", instructions="Inspect the node when requested."))
    model = call("Model.Register", dict(name="clock-provider", revision=str(seconds), lease_seconds=seconds,
                 max_in_flight=2, max_context_bytes=8192, max_output_bytes=1024, provider_principal_id="local-owner"))
    return call("Session.Open", dict(goal="Inspect local status", key=key, skill_ref=skill, model_ref=model,
                delegation_id="owner-delegation", context_policy="ReferencesOnly", max_turns=4))["reference"]["resource_id"]


def upgrade(previous, current):
    n = Node(previous)
    try:
        sid = open_session(n.call, "pending")
        done = open_session(n.call, "done")
        pending = dict(session_id=sid, key="previous")
        completed = dict(session_id=done, key="previous")
        decision = dict(action="finish", summary="observed")
        other = n.call("Model.Claim", completed)
        response = dict(session_id=done, lease_id=other["lease_id"], decision=decision)
        receipt = n.call("Model.Respond", response)
        lost = n.call("Model.Claim", pending)
        n.stop(signal.SIGKILL)
        old = n.daemon.copy()
        n.daemon[0] = str((current / "psiosd").resolve())
        report = json.loads(subprocess.check_output(n.daemon + ["verify"]))
        assert report["database_format"] == 4 and report["migration_required"], report
        n.start()
        assert n.call("Model.Claim", completed)["completed"] == receipt
        assert n.call("Model.Respond", response) == receipt
        for method, params in [("Model.Claim", pending), ("Model.Respond", dict(
                session_id=sid, lease_id=lost["lease_id"], decision=decision))]:
            try:
                n.call(method, params)
            except Failure as error:
                assert error.code == "Fenced", error
            else:
                raise AssertionError("format 4 pending model lease accepted")
        fresh = n.call("Model.Claim", dict(session_id=sid, key="current"))
        assert fresh["lease_id"] != lost["lease_id"] and fresh["turn"] == 2
        n.stop()
        downgrade = subprocess.run(old + ["serve"], capture_output=True, timeout=5)
        assert downgrade.returncode != 0 and b"SchemaVersionUnsupported" in downgrade.stderr
        n.start()
        assert n.call("Model.Claim", dict(session_id=sid, key="current"))["lease_id"] == fresh["lease_id"]
        assert n.call("Model.Respond", dict(session_id=sid, lease_id=fresh["lease_id"], decision=decision))["lifecycle"] == "Succeeded"
        print("PASS format 4 crash upgrade, fencing, retained decisions, downgrade refusal and same-boot restart")
    finally:
        n.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", required=True, type=Path)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
