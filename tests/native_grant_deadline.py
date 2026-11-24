"""Check real wall-clock rollback and durable grant expiry inside an isolated guest."""
import argparse
from datetime import datetime
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, password, stage_guest_chat_token
from native_static_ipv4 import installer, poweroff


def user_command(command):
    return "su -s /bin/sh psios -c " + shlex.quote(command)


def owner(console, method, parameters):
    payload = shlex.quote(json.dumps(parameters))
    return json.loads(console.run(user_command(f"printf '%s' {payload} | psiosctl call {method}")))


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
        console.send("exit")
        console.login("root", admin)

        wall = int(console.run("date -u +%s"))
        console.run(f"date -u -s '@{wall - 180}' >/dev/null")
        check = user_command(f"psiosctl --token-file {shlex.quote(credential)} status --delegation {shlex.quote(delegation)}")
        assert json.loads(console.run(check))["authority_id"] == node["resource_id"]
        for _ in range(4):
            finished = owner(console, "Task.Wait", {"id": task, "timeout_ms": 9000})
            if finished["lifecycle"] == "Failed":
                break
        assert (finished["lifecycle"], finished["outcome_knowledge"]) == ("Failed", "NotStarted"), finished
        assert finished["conditions"][0]["type"] == "DeadlineExceeded", finished
        denied = f"out=$({check} 2>&1); [ \"$?\" -ne 0 ] && printf '%s' \"$out\""
        assert "PermissionDenied" in console.run(denied)
        assert int(console.run("date -u +%s")) < datetime.fromisoformat(expires).timestamp()

        daemon = console.run("pidof psiosd")
        console.run(f"kill -TERM {daemon}")
        console.run(f"for i in $(seq 1 150); do pid=$(pidof psiosd) && [ \"$pid\" != {daemon} ] && "
            f"{user_command('psiosctl whoami')} >/dev/null 2>&1 && exit 0; sleep 0.1; done; exit 1")
        assert console.run("cat /proc/sys/kernel/random/boot_id") == boot_id
        assert "PermissionDenied" in console.run(denied)
        assert owner(console, "Task.Get", {"id": task}) == finished
        fresh = owner(console, "Grant.Issue", issue)["delegation_ref"]["resource_id"]
        reissued = user_command(f"psiosctl --token-file {shlex.quote(credential)} status --delegation {shlex.quote(fresh)}")
        assert json.loads(console.run(reissued))["authority_id"] == node["resource_id"]
        console.run("now=$(date -u +%s); date -u -s \"@$((now+180))\" >/dev/null")
        poweroff(console)
    check_clean_shutdown(disk)
    print("PASS real UTC rollback, elapsed grant and queued Task expiry, preserved denial after daemon restart")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-grant-deadline-") as directory:
        exercise(parser.parse_args(), Path(directory))
