"""Update the native Linux hostname on the verified persistent configuration."""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, check_state, password, sha
from native_upgrade import shutdown


def refused(console, command, message):
    output = console.run(f"{command} 2>&1; test $? -ne 0")
    assert message in output, output


def prove(console, operator, baseline):
    console.login("psios", operator)
    assert console.run("psios-hostname show && psios-hostname configured && uname -n").splitlines() == ["edge-final"] * 3
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
    common = ["--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive)]
    receipt.write_bytes(subprocess.check_output([sys.executable, "-B", args.installer, source, *common,
        "--console-hash", operator_hash, "--root-console-hash", admin_hash, "--hostname", "edge-initial"], timeout=180))
    receipt.chmod(0o600)

    with boot(work, source, pace_commands=True) as console:
        console.login("psios", operator)
        assert console.run("psios-hostname show && psios-hostname configured").splitlines() == ["edge-initial"] * 2
        console.run("grep -q ' /usr/bin/psios-hostname$' /psios-payload.sha256")
        refused(console, "psios-hostname set operator", "requires root")
        refused(console, "psios-hostname --boot", "requires the native init process")
        baseline = check_state(console)
        console.send("exit")
        console.login("root", admin)
        for invalid in ("-gone", "gone-", "other.example", "a" * 64):
            refused(console, "psios-hostname set " + shlex.quote(invalid), "single DNS label")
        refused(console, "( umask 077; exec 8>>/run/psios-hostname.lock; flock -x 8; psios-hostname set blocked )",
                "another hostname change is active")
        console.run("mv /etc/hostname /etc/hostname.prior && ln -s hostname.prior /etc/hostname")
        refused(console, "psios-hostname set redirected", "unsafe hostname file")
        assert console.run("cat /etc/hostname.prior") == "edge-initial"
        console.run("mv -T /etc/hostname.prior /etc/hostname")
        console.run("ln -s hostname /etc/.psios-hostname.pending")
        refused(console, "psios-hostname set redirected", "unsafe pending hostname file")
        console.run("rm /etc/.psios-hostname.pending; umask 077; printf 'cut-before-publish\\n' > /etc/.psios-hostname.pending")
        assert console.run("psios-hostname set edge-final && stat -c '%u:%g:%a:%h' /etc/hostname").splitlines() == [
            "edge-final", "0:0:644:1"]
        console.run("test ! -e /etc/.psios-hostname.pending")
        assert console.run("uname -n") == "edge-final"
        console.vm.kill()
        console.vm.wait(15)

    with boot(work, source, pace_commands=True) as console:
        old_boot = prove(console, operator, baseline)
        shutdown(console, admin)
    check_clean_shutdown(source)

    protected = {path: sha(path) for path in (source, receipt, code, variables)}
    staged = work / "staged"
    status = subprocess.check_output([sys.executable, "-B", args.controller, staged, *common,
        "--upgrade-from", source, "--previous-receipt", receipt, "--previous-receipt-sha256", protected[receipt],
        "--ovmf-code", code, "--ovmf-code-sha256", protected[code], "--ovmf-vars", variables,
        "--ovmf-vars-sha256", protected[variables]], timeout=500)
    assert json.loads(status)["trial"]["status"] == "Verified", status
    candidate = staged / "candidate.raw"
    shutil.copyfile(variables, work / "next-vars.fd")
    with boot(work, candidate, vars_file="next-vars.fd", pace_commands=True) as console:
        assert prove(console, operator, baseline) != old_boot
        shutdown(console, admin)
    check_clean_shutdown(candidate)
    assert {path: sha(path) for path in protected} == protected
    print("PASS native hostname changed live and preserved the hostname, original node and Task across a cut and offline update")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-hostname-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
