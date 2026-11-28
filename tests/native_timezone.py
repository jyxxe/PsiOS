"""Check offline native time zone selection and retention through an isolated upgrade."""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, check_state, password, sha
from native_upgrade import shutdown


def display(console, instant):
    seconds = int(datetime.fromisoformat(instant).replace(tzinfo=timezone.utc).timestamp())
    return console.run(f"unset TZ; date -d @{seconds} '+%H:%M %z %Z'")


def prove_shanghai(console, operator, baseline):
    console.login("psios", operator)
    assert console.run("psios-timezone show") == "Asia/Shanghai"
    assert console.run("readlink /etc/localtime") == "../usr/share/zoneinfo/Asia/Shanghai"
    assert display(console, "2026-03-08T07:01:00") == "15:01 +0800 CST"
    node, task, result, boot_id = check_state(console)
    assert node == baseline[0] and task["task_id"] == baseline[1]["task_id"] and result == baseline[2]
    assert task["deduplicated"] is True and boot_id != baseline[3]
    return boot_id


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    source = work / "source.raw"
    receipt = work / "source.json"
    code, variables = work / "code.fd", work / "vars.fd"
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", code)
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", variables)
    build = [sys.executable, "-B", args.installer, source, "--kernel-iso", args.kernel_iso,
             "--core-archive", args.previous_archive, "--core-sha256", sha(args.previous_archive),
             "--console-hash", operator_hash, "--root-console-hash", admin_hash]
    for invalid, message in (("../../etc/shadow", "invalid time zone name"),
                             ("Asia/Unknown_City", "unknown time zone")):
        failed = subprocess.run([*build, "--timezone", invalid], capture_output=True, text=True, timeout=90)
        assert failed.returncode != 0 and message in failed.stderr and not source.exists(), failed
    receipt.write_bytes(subprocess.check_output([*build, "--timezone", "America/New_York"], timeout=180))
    receipt.chmod(0o600)

    with boot(work, source, pace_commands=True) as console:
        console.login("psios", operator)
        assert console.run("psios-timezone") == "America/New_York"
        assert console.run("psios-timezone list | grep -x 'Europe/Berlin'") == "Europe/Berlin"
        assert console.run("stat -c '%u:%g:%a' /usr/share/zoneinfo/Europe/Berlin") == "0:0:444"
        assert display(console, "2026-03-08T06:59:00") == "01:59 -0500 EST"
        assert display(console, "2026-03-08T07:01:00") == "03:01 -0400 EDT"
        denied = console.run("psios-timezone set Asia/Shanghai 2>&1; test $? -ne 0")
        assert "requires root" in denied and console.run("psios-timezone show") == "America/New_York"
        baseline = check_state(console)
        console.send("exit")
        console.login("root", admin)
        before = int(console.run("date -u +%s"))
        for invalid in ("../UTC", "Asia//Shanghai", "Asia/Unknown_City"):
            denied = console.run(f"psios-timezone set {invalid} 2>&1; test $? -ne 0")
            assert "unknown IANA time zone" in denied
        assert console.run("psios-timezone show") == "America/New_York"
        assert console.run("psios-timezone set Asia/Shanghai") == "Asia/Shanghai"
        assert 0 <= int(console.run("date -u +%s")) - before < 60
        assert display(console, "2026-03-08T07:01:00") == "15:01 +0800 CST"
        console.send("poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(source)
    with boot(work, source, pace_commands=True) as console:
        old_boot = prove_shanghai(console, operator, baseline)
        shutdown(console, admin)

    protected = {path: sha(path) for path in (source, receipt, code, variables)}
    staged = work / "staged"
    status = subprocess.check_output([sys.executable, "-B", args.controller, staged,
        "--upgrade-from", source, "--previous-receipt", receipt, "--previous-receipt-sha256", protected[receipt],
        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive),
        "--ovmf-code", code, "--ovmf-code-sha256", protected[code],
        "--ovmf-vars", variables, "--ovmf-vars-sha256", protected[variables]], timeout=500)
    assert json.loads(status)["trial"]["status"] == "Verified", status
    candidate = staged / "candidate.raw"
    shutil.copyfile(variables, work / "next-vars.fd")
    with boot(work, candidate, vars_file="next-vars.fd", pace_commands=True) as console:
        assert prove_shanghai(console, operator, baseline) != old_boot
        shutdown(console, admin)
    check_clean_shutdown(candidate)
    assert {path: sha(path) for path in protected} == protected
    print("PASS offline IANA time zone, DST and UTC, root-only change, restart and isolated native upgrade")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for flag in ("--kernel-iso", "--previous-archive", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-timezone-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
