"""Settle a pre-metering reservation in place after a same-boot binary upgrade."""
import argparse
from contextlib import closing
from pathlib import Path
import signal
import sqlite3

from model_leases import open_session
from support import Failure, Node


def reject_unrelated_reservation(node, original, claim):
    node.stop(signal.SIGKILL)
    with closing(sqlite3.connect(node.state / "state.db")) as connection:
        identity = (original["resource_id"],)
        correlation, = connection.execute(
            "SELECT correlation FROM versions WHERE id=? AND rev=1", identity
        ).fetchone()
        with connection:
            connection.execute("UPDATE versions SET correlation='unrelated-request' WHERE id=? AND rev=1",
                               identity)
        try:
            node.start()
            try:
                node.call("Model.Claim", claim)
            except Failure as refused:
                assert refused.code == "OutcomeUnknown", refused
            else:
                raise AssertionError("unrelated reservation incorrectly authorized legacy replay")
        finally:
            node.stop()
            with connection:
                connection.execute("UPDATE versions SET correlation=? WHERE id=? AND rev=1",
                                   (correlation, *identity))
    node.start()


def upgrade(previous, current):
    node = Node(previous)
    try:
        sid = open_session(node.call, "upgrade")
        claim = dict(session_id=sid, key="prior")
        leased = node.call("Model.Claim", claim)
        assert "usage_format" not in leased
        original, = node.call("Resource.List", {"kind": "ModelUsage"})["items"]
        node.stop(signal.SIGKILL)
        node.daemon[0] = str((current / "psiosd").resolve())
        node.start()
        active = node.call("Model.Claim", claim)
        assert active["usage_format"] == "tokens.v1" and active["lease_id"] == leased["lease_id"]
        assert active["recovered"] is True and active["context"] == leased["context"]
        assert node.call("Resource.List", {"kind": "ModelUsage"})["items"] == [original]
        reject_unrelated_reservation(node, original, claim)
        assert node.call("Model.Claim", claim) == active
        counts = dict(input_tokens=201, output_tokens=8)
        response = dict(session_id=sid, lease_id=leased["lease_id"], decision=dict(action="finish", summary="done"), usage=counts)
        receipt = node.call("Model.Respond", response)
        updated = receipt["usage_ref"]
        assert updated["resource_id"] == original["resource_id"] and original["version"] == "r1" and updated["version"] == "r2"
        prior = node.call("Resource.Resolve", {"reference": original})["value"]
        settled = node.call("Resource.Resolve", {"reference": updated})["value"]
        assert prior["call_id"] == settled["call_id"] == leased["lease_id"]
        assert "reported_tokens" not in prior and settled["reported_tokens"] == counts
        assert settled["measurement_source"] == "ProviderReported"
        node.stop(signal.SIGKILL)
        node.start()
        assert node.call("Model.Respond", response) == node.call("Model.Claim", claim)["completed"] == receipt
        assert node.call("Resource.List", {"kind": "ModelUsage"})["items"] == [updated]
        budget = node.call("Budget.Get", {"id": settled["budget_ref"]["resource_id"]})
        assert budget["model_calls_used"] == 1
        print("PASS same-boot format 5 live reservation, immutable original revision, one settlement across restarts")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-bin", type=Path, required=True)
    parser.add_argument("--current-bin", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    upgrade(args.previous_bin, args.current_bin)
