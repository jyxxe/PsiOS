"""Check durable grant and Task expiry after wall-clock rollback in an isolated guest."""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

from native_combined_core import artifact, submit
from native_disk import REPO, boot, call, check_clean_shutdown, password, stage_guest_chat_token
from native_static_ipv4 import installer, poweroff


def user_command(command):
    return "su -s /bin/sh psios -c " + shlex.quote(command)


def owner_command(method, parameters):
    payload = shlex.quote(json.dumps(parameters))
    return user_command(f"printf '%s' {payload} | psiosctl call {method}")


def owner(console, method, parameters):
    return json.loads(console.run(owner_command(method, parameters)))


def rejection(console, command):
    return console.run(f"out=$({command} 2>&1); [ \"$?\" -ne 0 ] && printf '%s' \"$out\"")


def wait_expiry(console, task, revision=None):
    params = {"id": task, "timeout_ms": 9000}
    if revision is not None:
        params["after_version"] = revision
    for _ in range(4):
        finished = owner(console, "Task.Wait", params)
        if finished["lifecycle"] == "Failed":
            break
    assert (finished["lifecycle"], finished["outcome_knowledge"]) == ("Failed", "NotStarted"), finished
    assert finished["conditions"][0]["type"] == "DeadlineExceeded", finished
    return finished


def timed_write(console, volume, name, deadline):
    request = call(console, "Task.Prepare", {"tool": "volume.write", "key": name, "deadline_at": deadline,
        "delegation_id": "owner-delegation", "input": {"id": volume["resource_id"], "name": name,
        "content": "outside the approved window", "expected_resource_version": volume["version"]}})
    return call(console, "Task.Submit", request)


def exercise(args, work):
    user, operator_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", work / "code.fd")
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", work / "vars.fd")
    disk = work / "grant-deadline.raw"
    subprocess.run([*installer(args, disk, args.archive), "--console-hash", operator_hash,
        "--root-console-hash", root_hash], stdout=subprocess.DEVNULL, check=True, timeout=150)
    with boot(work, disk) as console:
        console.login("psios", user)
        volume = artifact(console, submit(console, "volume.create", {"name": "elapsed-effects"}))
        node = call(console, "Node.Get", {})["reference"]
        call(console, "Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
        boot_id = console.run("cat /proc/sys/kernel/random/boot_id")
        agent = call(console, "Agent.Register", {"name": "elapsed-grant"})
        credential = stage_guest_chat_token(console, agent["token"])
        issue = {"principal_id": agent["principal_id"], "units": 2, "ttl_seconds": 22,
                 "actions": ["system.status", "task.submit", "task.read"], "tool_targets": ["profile-local"]}
        grant = call(console, "Grant.Issue", issue)
        grant_id = grant["grant_ref"]["resource_id"]
        expires = call(console, "Resource.Get", {"id": grant_id})["value"]["valid_until"]
        delegation = grant["delegation_ref"]["resource_id"]
        request = call(console, "Task.Prepare", {"tool": "system.status", "input": {}, "key": "elapsed-window",
            "delegation_id": delegation}, credential)
        task = call(console, "Task.Submit", request, credential)["task_id"]
        assert call(console, "Task.Get", {"id": task})["lifecycle"] == "Queued"
        task_expiry = int(console.run("date -u +%s")) + 27
        deadline = datetime.fromtimestamp(task_expiry, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        timed = timed_write(console, volume, "missed-approval", deadline)
        pending = call(console, "Task.Get", {"id": timed["task_id"]})
        assert pending["lifecycle"] == "WaitingApproval", pending
        approval_id = pending["conditions"][0]["message"]
        orig = timed_write(console, volume, "retry-budget", deadline)["task_id"]
        original_version = call(console, "Resource.Get", {"id": orig})["reference"]["version"]
        call(console, "Task.Cancel", {"id": orig, "expected_resource_version": original_version})
        console.send("exit")
        console.login("root", admin)

        wall = int(console.run("date -u +%s"))
        console.run(f"date -u -s '@{wall - 180}' >/dev/null")
        check = user_command(f"psiosctl --token-file {shlex.quote(credential)} status --delegation {shlex.quote(delegation)}")
        assert json.loads(console.run(check))["authority_id"] == node["resource_id"]
        cancelled = owner(console, "Resource.Get", {"id": orig})["reference"]["version"]
        retry_params = {"id": orig, "expected_resource_version": cancelled, "key": "retry-before-cutoff"}
        retried = owner(console, "Task.Retry", retry_params)
        assert owner(console, "Task.Get", {"id": retried["task_id"]})["retry_of_task_id"] == orig
        finished = wait_expiry(console, task)
        assert "PermissionDenied" in rejection(console, check)
        wait_expiry(console, timed["task_id"], timed["task_revision"])
        wait_expiry(console, retried["task_id"], retried["task_revision"])
        req = owner(console, "Resource.Get", {"id": approval_id})
        assert req["value"]["status"] == "Expired" and "decision_ref" not in req["value"], req
        approve = {"id": approval_id, "expected_resource_version": req["reference"]["version"],
                   "plan_digest": req["value"]["plan_digest"], "approve": True}
        assert "ApprovalInvalidated" in rejection(console, owner_command("Approval.Decide", approve))
        refused = dict(retry_params, key="retry-after-cutoff")
        assert "DeadlineExceeded" in rejection(console, owner_command("Task.Retry", refused))
        assert owner(console, "Task.Retry", retry_params)["task_id"] == retried["task_id"]
        assert int(console.run("date -u +%s")) < datetime.fromisoformat(expires).timestamp()
        assert int(console.run("date -u +%s")) < task_expiry

        daemon = console.run("pidof psiosd")
        console.run(f"kill -TERM {daemon}")
        console.run(f"for i in $(seq 1 150); do pid=$(pidof psiosd) && [ \"$pid\" != {daemon} ] && "
            f"{user_command('psiosctl whoami')} >/dev/null 2>&1 && exit 0; sleep 0.1; done; exit 1")
        assert console.run("cat /proc/sys/kernel/random/boot_id") == boot_id
        assert "PermissionDenied" in rejection(console, check)
        assert owner(console, "Task.Get", {"id": task}) == finished
        fresh = owner(console, "Grant.Issue", issue)["delegation_ref"]["resource_id"]
        reissued = user_command(f"psiosctl --token-file {shlex.quote(credential)} status --delegation {shlex.quote(fresh)}")
        assert json.loads(console.run(reissued))["authority_id"] == node["resource_id"]
        cordoned = owner(console, "Node.Get", {})["reference"]
        owner(console, "Node.Configure", {"expected_resource_version": cordoned["version"], "scheduling": "Enabled"})
        directory = shlex.quote(f"/var/lib/psios/volumes/{volume['resource_id']}/data")
        console.run(f"cd {directory} && [ ! -e missed-approval ] && [ ! -e retry-budget ]")
        console.run("now=$(date -u +%s); date -u -s \"@$((now+180))\" >/dev/null")
        poweroff(console)
    check_clean_shutdown(disk)
    print("PASS real UTC rollback, elapsed grant and Task expiry, approval and retry fencing, daemon restart")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-grant-deadline-") as directory:
        exercise(parser.parse_args(), Path(directory))
