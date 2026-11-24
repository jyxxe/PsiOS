"""Use a real installed Core across both instances of a repeated local minute."""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

from native_disk import REPO, boot, call, password, sha


def wait_trigger(console, identity, count):
    limit = time.monotonic() + 15
    while True:
        state = call(console, "Schedule.Get", identity)["state"]
        if len(state["trigger_refs"]) == count:
            return state
        assert time.monotonic() < limit, state
        time.sleep(0.1)


def task_for_trigger(console, ref, identity, ordinal, window):
    report = call(console, "Schedule.TriggerGet", {"id": ref["resource_id"]})
    state = report["state"]
    assert report["reference"] == ref and state["ordinal"] == ordinal, report
    assert all(state[k] == v for k, v in window.items()), report
    assert state["lifecycle"] == "Triggered", report
    receipt = state["task_receipt"]
    assert receipt["origin_ref"]["origin_id"] == identity["id"], receipt
    assert receipt["idempotency_key"] == ref["resource_id"], receipt
    task = json.loads(console.run("psiosctl wait " + shlex.quote(receipt["task_id"])))
    assert task["lifecycle"] == "Succeeded", task
    return receipt


def exercise(args, work):
    operator, op_hash = password(work, "operator.hash")
    _, root_hash = password(work, "root.hash")
    image = work / "calendar.raw"
    subprocess.run([sys.executable, args.installer.resolve(), image, "--kernel-iso", args.kernel_iso,
                    "--core-archive", args.archive, "--core-sha256", sha(args.archive),
                    "--console-hash", op_hash, "--root-console-hash", root_hash],
                   stdout=subprocess.DEVNULL, check=True)
    for name, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / name, work / target)

    # Set only each VM's RTC. Shut off the real guest between every test stage.
    local_date = "2026-11-01"
    with boot(work, image, rtc_base=f"{local_date}T05:00:00") as console:
        console.login("psios", operator)
        grant = call(console, "Grant.Issue", {
            "principal_id": "local-owner", "tool_targets": ["profile-local"], "ttl_seconds": 7200,
            "actions": ["schedule.manage", "task.submit", "task.read", "system.status"], "units": 2,
        })
        request = {
            "key": "native-dst-fold", "tool": "system.status", "input": {},
            "delegation_id": grant["delegation_ref"]["resource_id"],
            "calendar": {"timezone": "America/New_York", "local_time": "01:30:00", "start_date": local_date,
                         "days": 1, "window_seconds": 120, "fold": "Both", "gap": "Reject",
                         "missed": {"mode": "BoundedCatchUp", "limit": 2}, "overlap": "Skip"},
        }
        pending = call(console, "Schedule.Create", request)
        identity = {"id": pending["reference"]["resource_id"]}
        plan = pending["state"]["calendar_plan"]
        first, second = plan["windows"]
        assert first == {"run_at": f"{local_date}T05:30:00Z", "deadline_at": f"{local_date}T05:32:00Z"}, plan
        assert second == {"run_at": f"{local_date}T06:30:00Z", "deadline_at": f"{local_date}T06:32:00Z"}, plan
        assert plan["timezone_database"] and plan["skipped_dates"] == []
        assert pending["state"]["lifecycle"] == "Pending" and pending["state"]["trigger_refs"] == []

    with boot(work, image, rtc_base=f"{local_date}T05:30:02") as console:
        console.login("psios", operator)
        state = wait_trigger(console, identity, 1)
        first_ref, = state["trigger_refs"]
        assert first_ref["resource_id"] == pending["state"]["next"]["trigger_id"], state
        first_receipt = task_for_trigger(console, first_ref, identity, 1, first)
        assert state["next"]["run_at"] == second["run_at"] and state["lifecycle"] == "Pending", state
        assert state["calendar_plan"] == plan, state

    with boot(work, image, rtc_base=f"{local_date}T06:30:02") as console:
        console.login("psios", operator)
        state = wait_trigger(console, identity, 2)
        again, second_ref = state["trigger_refs"]
        assert again == first_ref and second_ref != first_ref, state
        assert task_for_trigger(console, again, identity, 1, first) == first_receipt
        second_receipt = task_for_trigger(console, second_ref, identity, 2, second)
        assert second_receipt["task_id"] != first_receipt["task_id"], second_receipt
        assert state["lifecycle"] == "Completed" and state["next"] is None, state
        assert state["calendar_plan"] == plan and call(console, "Schedule.Create", request)["state"] == state
        budget = call(console, "Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        assert (budget["reserved"], budget["spent"]) == (0, 2), budget
        assert len(call(console, "Resource.List", {"kind": "Task"})["items"]) == 2
    print("PASS both DST fold instants on a real UEFI disk with two power cuts and distinct durable Task receipts")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-calendar-test-") as directory:
        exercise(parser.parse_args(), Path(directory))
