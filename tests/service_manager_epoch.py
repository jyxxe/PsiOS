"""Reject a new systemd user manager as evidence for an old acknowledged control."""
import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import pwd
import select
import shlex
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile

from support import Failure, Node, key, target


MANAGER_PROPERTY = "UserspaceTimestampMonotonic"
MANAGER_EPOCH = "manager_started_monotonic_us"
BOOT_ID = Path("/proc/sys/kernel/random/boot_id")


def command(*args, check=True, timeout=25):
    result = subprocess.run(args, timeout=timeout, capture_output=True, text=True)
    if check:
        assert result.returncode == 0, result
    return result


def rejects(code, node, method, params):
    try:
        node.call(method, params)
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"{method} unexpectedly succeeded")


def wait_byte(fd, timeout, label):
    assert select.select([fd], [], [], timeout)[0], f"timed out waiting for {label}"
    assert os.read(fd, 1) == b"1", f"missing {label}"


def inspect(node, service):
    task = node.wait(node.submit("service.inspect", target(service)))
    assert task["lifecycle"] == "Succeeded", task
    evidence, = task["result"]["artifact_refs"]
    return evidence, node.call("Artifact.Read", {"reference": evidence})


def worker(binaries, uid, notify, resumed):
    assert uid != 0 and os.geteuid() == uid
    account = pwd.getpwuid(uid)
    runtime = Path(f"/run/user/{uid}")
    private = (runtime / "systemd" / "private").stat()
    assert runtime.stat().st_uid == uid and stat.S_IMODE(runtime.stat().st_mode) == 0o700
    assert private.st_uid == uid and stat.S_ISSOCK(private.st_mode), private
    assert os.environ["XDG_RUNTIME_DIR"] == str(runtime) and "DBUS_SESSION_BUS_ADDRESS" not in os.environ
    ctl = ("/usr/bin/systemctl", "--user", "--no-pager", "--no-ask-password")

    def epoch():
        value = command(*ctl, "show", f"--property={MANAGER_PROPERTY}", "--value").stdout.strip()
        assert value.isascii() and value.isdecimal() and int(value) > 0 and str(int(value)) == value, value
        return value

    node = Node(binaries)
    unit = f"psios-manager-epoch-{key()[:12]}.service"
    service_id = f"service-{unit}"
    units = Path(account.pw_dir) / ".config" / "systemd" / "user"
    definition = units / unit
    invocations = node.base / "invocations"
    launch = node.base / "launch.sh"
    original_boot = BOOT_ID.read_text().strip()
    try:
        units.mkdir(parents=True, mode=0o700, exist_ok=True)
        launch.write_text(f"set -eu\nprintf '%s\\n' \"$INVOCATION_ID\" >> {shlex.quote(str(invocations))}\n")
        definition.write_text(f"[Unit]\nDescription=Isolated PsiOS manager epoch fixture\n"
                              f"[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/sh {json.dumps(str(launch))}\n")
        command(*ctl, "daemon-reload")
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [{"unit": unit, "manager": "user", "allow_control": False}]
        node.config.write_text(json.dumps(config))
        node.start()
        _, before = inspect(node, node.get(service_id)["reference"])
        original_epoch = epoch()
        assert (before[MANAGER_EPOCH], before["active_state"], before["manager_uid"], before["boot_id"]) == (
            original_epoch, "inactive", uid, original_boot,
        ), before
        node.stop()
        config["services"][0].update(allow_control=True, definition_digest=before["definition_digest"])
        node.config.write_text(json.dumps(config))
        node.start()
        service = node.get(service_id)["reference"]

        # Abort only the receipt transaction after the real control returned and was observed.
        database = node.state / "state.db"
        with closing(sqlite3.connect(database, timeout=10)) as db, db:
            db.execute("""CREATE TRIGGER lose_acknowledged_control BEFORE INSERT ON versions
                WHEN NEW.kind='HostServiceControlConfirmation'
                BEGIN SELECT RAISE(ABORT, 'fixture loses acknowledged systemd receipt'); END""")
        request = node.prepare("service.start", target(service), submission_key="original-systemd-control")
        task_id = node.call("Task.Submit", request)["task_id"]
        paused = node.wait(task_id, autoapprove=True)
        assert (paused["lifecycle"], paused["outcome_knowledge"], paused["conditions"][0]["type"]) == (
            "Paused", "Unknown", "OutcomeUnknown",
        ), paused
        assert paused["result"]["artifact_refs"] == []
        command(*ctl, "is-active", "--quiet", unit)
        original_invocation, = invocations.read_text().splitlines()
        operation, = paused["operation_refs"]
        operation_id = operation["resource_id"]
        old_evidence, old_observation = inspect(node, service)
        assert (old_observation[MANAGER_EPOCH], old_observation["active_state"], old_observation["invocation_id"]) == (
            original_epoch, "active", original_invocation,
        ), old_observation

        node.stop(signal.SIGKILL)
        with closing(sqlite3.connect(database, timeout=10)) as db, db:
            db.execute("DROP TRIGGER lose_acknowledged_control")
            raw, = db.execute("""SELECT body FROM current WHERE kind='HostServiceControlCheckpoint'
                AND spec->>'operation_id'=?""", (operation_id,)).fetchone()
            checkpoint = json.loads(raw)
            assert checkpoint["before"][MANAGER_EPOCH] == original_epoch, checkpoint
            effect_ref = checkpoint["effect_ref"]
            revision = int(effect_ref["version"][1:])
            assert db.execute("SELECT body->>'phase' FROM versions WHERE id=? AND rev=?",
                              (effect_ref["resource_id"], revision)).fetchone() == ("EffectStarted",)
            assert db.execute("SELECT holder FROM holds WHERE volume=?", (service_id,)).fetchone() == (operation_id,)
            assert not db.execute("""SELECT 1 FROM current WHERE kind='HostServiceControlConfirmation'
                                  AND spec->>'operation_id'=?""", (operation_id,)).fetchone()

        os.write(notify, b"1")
        os.close(notify)
        wait_byte(resumed, 55, "replacement user manager")
        os.close(resumed)
        restarted_epoch = epoch()
        assert int(restarted_epoch) > int(original_epoch), (original_epoch, restarted_epoch)
        assert BOOT_ID.read_text().strip() == original_boot
        command(*ctl, "daemon-reload")
        assert command(*ctl, "show", "--property=ActiveState", "--value", "--", unit).stdout.strip() == "inactive"
        command(*ctl, "start", "--", unit)
        first, replacement = invocations.read_text().splitlines()
        assert first == original_invocation != replacement

        node.start()
        assert node.wait(task_id) == paused
        assert node.get(service_id)["reference"] == service
        evidence, observed = inspect(node, service)
        assert (observed[MANAGER_EPOCH], observed["active_state"], observed["invocation_id"]) == (
            restarted_epoch, "active", replacement,
        ), observed
        for field in ("unit", "manager", "manager_uid", "boot_id", "definition_digest"):
            assert observed[field] == old_observation[field], (field, observed, old_observation)
        for reference in (old_evidence, evidence):
            rejects("OutcomeUnknown", node, "Operation.Reconcile", {**target(operation), "evidence_ref": reference})
            assert node.wait(task_id) == paused
        same = node.call("Task.Submit", request)
        assert same["deduplicated"] and same["task_id"] == task_id, same
        rejects("OutcomeUnknown", node, "Task.Retry", {**target(node.get(task_id)["reference"]), "key": key()})
        blocked = node.wait(node.submit("service.stop", target(service)), autoapprove=True)
        assert (blocked["lifecycle"], blocked["outcome_knowledge"], blocked["conditions"][0]["type"]) == (
            "Failed", "NotStarted", "Conflict",
        ), blocked
        assert invocations.read_text().splitlines() == [original_invocation, replacement]
        assert node.get(operation_id)["reference"] == operation
        node.stop()
        with closing(sqlite3.connect(database, timeout=10)) as db:
            assert db.execute("SELECT holder FROM holds WHERE volume=?", (service_id,)).fetchone() == (operation_id,)
            assert db.execute("SELECT phase FROM queue WHERE task=?", (task_id,)).fetchone() == ("Unknown",)
        verified = json.loads(command(*node.daemon, "verify").stdout)
        assert verified["database_format"] >= 77 and not verified["migration_required"], verified
        print("PASS same-boot manager replacement fences original and replacement inspection evidence", flush=True)
    finally:
        try:
            command(*ctl, "stop", "--", unit, check=False)
        finally:
            definition.unlink(missing_ok=True)
            node.close()


def supervise(binaries):
    assert os.geteuid() == 0, "use sudo on a disposable systemd CI runner"
    system = ("/usr/bin/systemctl", "--system", "--no-pager", "--no-ask-password")
    caller_uid = int(os.environ.get("SUDO_UID", "0"))
    original_boot = BOOT_ID.read_text().strip()
    with tempfile.TemporaryDirectory(dir="/tmp", prefix="psios-manager-epoch-") as temp:
        root = Path(temp)
        home = root / "home"
        home.mkdir(mode=0o700)
        scratch_bin = root / "bin"
        scratch_bin.mkdir(mode=0o755)
        daemon = scratch_bin / "psiosd"
        shutil.copyfile(binaries / "psiosd", daemon)
        daemon.chmod(0o755)
        for source in (Path(__file__), Path(__file__).with_name("support.py")):
            copied = root / source.name
            shutil.copyfile(source, copied)
            copied.chmod(0o644)
        username = f"psios-epoch-{key()[:12]}"
        command("/usr/sbin/useradd", "--no-create-home", "--user-group", "--home-dir", str(home),
                "--shell", "/usr/sbin/nologin", "--", username)
        account = None
        manager = None
        ready_in = ready_out = resume_in = resume_out = None
        process = None
        try:
            account = pwd.getpwnam(username)
            uid, gid = account.pw_uid, account.pw_gid
            assert uid not in (0, caller_uid) and account.pw_dir == str(home), account
            manager = f"user@{uid}.service"
            os.chown(root, 0, gid)
            root.chmod(0o750)
            os.chown(home, uid, gid)
            ready_in, ready_out = os.pipe()
            resume_in, resume_out = os.pipe()
            command("/usr/bin/loginctl", "enable-linger", username)
            command(*system, "start", manager)
            first_pid = int(command(*system, "show", "--property=MainPID", "--value", manager).stdout)
            assert first_pid > 1
            process = subprocess.Popen(
                [sys.executable, "-B", str(root / Path(__file__).name), "--bin-dir", str(scratch_bin),
                 "--worker", str(uid), str(ready_out), str(resume_in)],
                user=uid, group=gid, extra_groups=(), cwd=home, start_new_session=True,
                env={"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C", "HOME": str(home),
                     "XDG_RUNTIME_DIR": f"/run/user/{uid}"}, pass_fds=(ready_out, resume_in),
            )
            os.close(ready_out)
            ready_out = None
            os.close(resume_in)
            resume_in = None
            wait_byte(ready_in, 65, "durable unknown systemd control")
            assert pwd.getpwnam(username) == account
            assert int(command(*system, "show", "--property=MainPID", "--value", manager).stdout) == first_pid
            command(*system, "stop", manager)
            assert command(*system, "show", "--property=MainPID", "--value", manager).stdout.strip() == "0"
            command(*system, "start", manager)
            second_pid = int(command(*system, "show", "--property=MainPID", "--value", manager).stdout)
            assert second_pid > 1 and second_pid != first_pid and BOOT_ID.read_text().strip() == original_boot
            os.write(resume_out, b"1")
            assert process.wait(60) == 0
        finally:
            for fd in (ready_in, ready_out, resume_in, resume_out):
                if fd is not None:
                    os.close(fd)
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(5)
            assert account is None or pwd.getpwnam(username) == account
            if manager is not None:
                command("/usr/bin/loginctl", "disable-linger", username, check=False)
                command(*system, "stop", manager, f"user-runtime-dir@{uid}.service", check=False)
            command("/usr/sbin/userdel", "--", username)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--worker", nargs=3, type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.worker is None:
        supervise(args.bin_dir)
    else:
        worker(args.bin_dir, *args.worker)
