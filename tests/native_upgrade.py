"""Copy an installed format 5 UEFI node and boot its independently upgraded disk."""
import argparse
from contextlib import closing
import json
from pathlib import Path
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile

from native_disk import boot, call, check_native_network, check_state, extract_partition, password, replace_partition, sha


TRIAL = b"PSIOS-TRIAL/1 "
SERVICE_MARKER = "PSIOS-PERSISTED-SERVICE-RAN"
STATE_OFFSET = 65
STATE_MIB = 192


def shutdown(console, admin):
    console.send("exit")
    console.login("root", admin)
    console.send("cd /; exec poweroff")
    assert console.vm.wait(35) == 0


def add_migration_conflict(work, disk):
    partition = work / "conflicting-state.ext4"
    source = "/psios/state.db"
    db = work / "conflicting-state.db"
    extract_partition(disk, partition, STATE_OFFSET, STATE_MIB)
    subprocess.run(["debugfs", "-R", f"dump {source} {db.name}", partition], cwd=work,
                   check=True, capture_output=True, timeout=20)
    con = sqlite3.connect(db)
    try:
        with con:
            # The old schema permits this index, which conflicts with the target migration.
            assert con.execute("PRAGMA user_version").fetchone() == (5,)
            con.execute("CREATE INDEX versions_origin_seq ON versions(seq)")
            assert con.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
    finally:
        con.close()
    patch = (f"rm {source}\nwrite {db.name} {source}\nset_inode_field {source} mode 0100600\n"
             f"set_inode_field {source} uid 1000\nset_inode_field {source} gid 1000\n")
    subprocess.run(["debugfs", "-w", "-f", "-", partition], cwd=work, input=patch,
                   check=True, capture_output=True, text=True, timeout=20)
    replace_partition(disk, partition, STATE_OFFSET)


def trial_rejected_overlay(work, trial_disk):
    """Change the saved configuration only in this disposable boot view."""
    altered = work / "trial-altered.raw"
    state = work / "trial-altered.ext4"
    config = work / "trial-altered.json"
    source = "/etc/psios/core.json"
    subprocess.run(["qemu-img", "convert", "-q", "-m", "1", "-f", "raw", "-O", "raw", trial_disk, altered],
                   check=True, timeout=90)
    extract_partition(altered, state, STATE_OFFSET, STATE_MIB)
    subprocess.run(["debugfs", "-R", f"dump {source} {config.name}", state], cwd=work,
                   check=True, capture_output=True, timeout=20)
    config.write_text(json.dumps(dict(json.loads(config.read_text()), workers=3)))
    patch = (f"rm {source}\nwrite {config.name} {source}\nset_inode_field {source} mode 0100644\n"
             f"set_inode_field {source} uid 0\nset_inode_field {source} gid 0\n")
    subprocess.run(["debugfs", "-w", "-f", "-", state], cwd=work, input=patch,
                   check=True, capture_output=True, text=True, timeout=20)
    replace_partition(altered, state, STATE_OFFSET)
    overlay = work / "trial-rejected.qcow2"
    subprocess.run(["qemu-img", "convert", "-q", "-m", "1", "-f", "raw", "-O", "qcow2",
                    "-B", str(trial_disk), "-F", "raw", altered, overlay], check=True, timeout=90)
    altered.unlink()
    state.unlink()
    config.unlink()
    return overlay


def trial_report(console):
    before = console.until(TRIAL, timeout=170)
    report = json.loads(console.until(b"\n"))
    tail, _ = console.vm.communicate(timeout=35)
    rest = console.buf + tail
    assert console.vm.returncode == 0, rest[-512:]
    assert TRIAL not in rest, rest[-512:]
    transcript = before + rest
    for unexpected in (SERVICE_MARKER.encode(), b"login: ", b"psiosd ready", b"udhcpc:"):
        assert unexpected not in transcript, transcript[-1024:]
    return report


def boot_trial(work, disk, original_vars, expected, *, disk_format="qcow2"):
    private_vars = disk.with_suffix(".fd")
    shutil.copyfile(original_vars, private_vars)
    with boot(work, disk, disk_format=disk_format, vars_file=private_vars.name) as c:
        report = trial_report(c)
        assert report == expected, report


def migrated_trial_state(work, overlay, expected_format, node_id):
    raw = work / "trial-verified.raw"
    state = work / "trial-verified.ext4"
    database = work / "trial-verified.db"
    subprocess.run(["qemu-img", "convert", "-q", "-m", "1", "-f", "qcow2", "-O", "raw", overlay, raw],
                   check=True, timeout=90)
    extract_partition(raw, state, STATE_OFFSET, STATE_MIB)
    subprocess.run(["e2fsck", "-f", "-n", state], check=True, capture_output=True, timeout=45)
    subprocess.run(["debugfs", "-R", f"dump /psios/state.db {database.name}", state], cwd=work,
                   check=True, capture_output=True, timeout=20)
    absent = subprocess.run(["debugfs", "-R", "stat /native-service-ran", state], capture_output=True, timeout=20)
    assert b"File not found" in absent.stderr, absent
    with closing(sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True)) as db:
        assert db.execute("PRAGMA user_version").fetchone() == (expected_format,)
        assert db.execute("SELECT value FROM meta WHERE key='authority'").fetchone() == (node_id,)
        assert db.execute("PRAGMA quick_check").fetchall() == [("ok",)]
    raw.unlink()
    state.unlink()
    database.unlink()


def exercise_trial(work, prepare, previous, original_vars, node_id, config_digest):
    disk = work / "trial.raw"
    command = [*prepare, "--trial-boot"]
    command[2] = disk
    receipt = json.loads(subprocess.check_output(command, timeout=330))
    assert (receipt["format"], receipt["boot_mode"]) == (7, "IsolatedTrial"), receipt
    assert sha(disk) == receipt["initial_image_sha256"]
    rehearsal = receipt["upgrade"]["verified_authority"]["migration_rehearsal"]
    assert rehearsal["source_format"] == 5 < rehearsal["target_format"], rehearsal
    assert rehearsal["configuration_sha256"] == config_digest, rehearsal

    overlay = work / "trial.qcow2"
    subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw", "-b", str(disk), overlay],
                   check=True, timeout=30)
    boot_trial(work, overlay, original_vars, {
        "status": "Ready", "deployment_manifest_sha256": receipt["deployment_manifest_sha256"],
    })
    migrated_trial_state(work, overlay, rehearsal["target_format"], node_id)

    rejected = trial_rejected_overlay(work, disk)
    boot_trial(work, rejected, original_vars, {"status": "Failed", "stage": "trial-configuration"})

    transplanted = work / "trial-transplanted.raw"
    esp = work / "trial-transplanted.fat"
    subprocess.run(["qemu-img", "convert", "-q", "-m", "1", "-f", "raw", "-O", "raw", previous, transplanted],
                   check=True, timeout=90)
    extract_partition(disk, esp, 1, 64)
    replace_partition(transplanted, esp, 1)
    transplanted_digest = sha(transplanted)
    boot_trial(work, transplanted, original_vars, {"status": "Failed", "stage": "trial-disk"}, disk_format="raw")
    assert sha(transplanted) == transplanted_digest
    transplanted.unlink()
    esp.unlink()
    assert sha(disk) == receipt["initial_image_sha256"]

    receipt_path = work / "trial.json"
    receipt_path.write_text(json.dumps(receipt))
    blocked = work / "trial-must-not-activate.raw"
    promote = prepare.copy()
    promote[2], promote[4], promote[6] = blocked, disk, receipt_path
    failure = subprocess.run(promote, capture_output=True, text=True, timeout=95)
    assert failure.returncode != 0 and "trial" in failure.stderr.lower() and not blocked.exists(), failure
    print("PASS actual native trial migration, isolated services, config and disk-binding refusal, unchanged backing", flush=True)


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    previous = work / "previous.raw"
    next_disk = work / "next.raw"
    previous_receipt = work / "previous.json"
    for source, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / source, work / target)
    build = subprocess.check_output([sys.executable, args.previous_installer, previous,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.previous_archive,
        "--core-sha256", sha(args.previous_archive), "--console-hash", operator_hash,
        "--root-console-hash", admin_hash], timeout=180)
    previous_receipt.write_bytes(build)
    old = json.loads(build)
    prepare = [sys.executable, args.installer, next_disk, "--upgrade-from", previous,
               "--previous-receipt", previous_receipt, "--kernel-iso", args.kernel_iso,
               "--core-archive", args.archive, "--core-sha256", sha(args.archive)]

    with boot(work, previous) as c:
        c.login("psios", operator)
        first = check_state(c)
        submitted = json.loads(c.run("printf '{\"name\":\"native-upgrade\"}' | psiosctl submit volume.create --key native-upgrade-volume"))
        task = json.loads(c.run("psiosctl wait " + shlex.quote(submitted["task_id"])))
        assert task["lifecycle"] == "Succeeded", task
        volume = call(c, "Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
        volume_file = shlex.quote(f"/var/lib/psios/volumes/{volume['resource_id']}/data/upgrade-marker")
        c.run(f"printf 'retained-volume\\n' > {volume_file}; sync -f {volume_file}")
        c.run("echo retained-home > before-upgrade; sync -f .")
        refused = subprocess.run(prepare, capture_output=True, text=True, timeout=95)
        assert refused.returncode != 0 and "lock" in refused.stderr and not next_disk.exists(), refused
        c.send("exit")
        c.login("root", admin)
        c.run("sed -i 's/^{/{\"workers\":2,/' /etc/psios/core.json")
        core_config_digest = c.run("sha256sum /etc/psios/core.json").split()[0]
        inittab = f"::once:/bin/sh -c 'echo {SERVICE_MARKER} > /dev/console; touch /var/lib/native-service-ran'"
        c.run(f"printf '%s\\n' {shlex.quote(inittab)} >> /etc/inittab")
        c.run("echo retained-admin > /root/maintenance; sync")
        # Capture a crash-consistent image with real outstanding ext4 journals.
        c.vm.kill()
        assert c.vm.wait(15) < 0

    old_digest = sha(previous)
    original_vars = work / "vars.fd"
    original_vars_digest = sha(original_vars)
    previous_receipt.write_text(json.dumps(dict(old, efi_application_sha256="0" * 64)))
    refused = subprocess.run(prepare, capture_output=True, text=True, timeout=95)
    assert refused.returncode != 0 and "EFI application" in refused.stderr and not next_disk.exists(), refused
    previous_receipt.write_text(json.dumps(dict(old, home_uuid="00000000-0000-4000-8000-000000000001")))
    refused = subprocess.run(prepare, capture_output=True, text=True, timeout=95)
    assert refused.returncode != 0 and "previous deployment UUID" in refused.stderr and not next_disk.exists(), refused
    assert sha(previous) == old_digest
    previous_receipt.write_bytes(build)

    exercise_trial(work, prepare, previous, original_vars, first[0], core_config_digest)
    assert sha(previous) == old_digest and sha(original_vars) == original_vars_digest

    upgrade = json.loads(subprocess.check_output(prepare, timeout=330))
    verified = upgrade["upgrade"]["verified_authority"]
    assert (verified["node_id"], verified["database_format"], verified["migration_required"]) == (first[0], 5, True), verified
    rehearsal = verified["migration_rehearsal"]
    assert rehearsal["status"] == "Verified" and rehearsal["source_format"] == 5 < rehearsal["target_format"], rehearsal
    assert rehearsal["configuration_sha256"] == core_config_digest, rehearsal
    assert upgrade["state_uuid"] == old["state_uuid"] and upgrade["home_uuid"] == old["home_uuid"]
    assert upgrade["upgrade"]["previous_image_sha256"] == sha(previous) == old_digest
    assert upgrade["upgrade"]["previous_source_commit"] == old["source_commit"] != upgrade["source_commit"]
    assert upgrade["initial_image_sha256"] == sha(next_disk)
    assert next_disk.stat().st_mode & 0o077 == 0
    assert upgrade["upgrade"]["replayed_ext4_partitions"] == [2, 3]

    active_vars = work / "candidate-vars.fd"
    shutil.copyfile(original_vars, active_vars)
    with boot(work, next_disk, vars_file=active_vars.name) as c:
        c.login("psios", operator)
        assert c.run("sha256sum /psios-deployment.json").split()[0] == upgrade["deployment_manifest_sha256"]
        assert c.run("cat /psios-source") == upgrade["source_commit"]
        after = check_state(c)
        assert after[0] == first[0] and after[1]["task_id"] == first[1]["task_id"] and after[2] == first[2]
        assert after[1]["deduplicated"] and after[3] != first[3]
        assert c.run("cat before-upgrade") == "retained-home"
        assert call(c, "Resource.Get", {"id": volume["resource_id"]})["reference"] == volume
        assert c.run(f"cat {volume_file}") == "retained-volume"
        assert json.loads(c.run("psiosctl node"))["state"]["configuration"]["worker_count"] == 2
        check_native_network(c)
        c.send("exit")
        c.login("root", admin)
        assert c.run("sha256sum /etc/psios/core.json").split()[0] == core_config_digest
        assert c.run("cat maintenance") == "retained-admin"
        c.run("test -f /var/lib/native-service-ran")
        c.send("poweroff")
        assert c.vm.wait(35) == 0

    assert sha(previous) == old_digest and sha(original_vars) == original_vars_digest
    with boot(work, previous) as c:
        c.login("psios", operator)
        assert c.run("cat /psios-source") == old["source_commit"]
        assert c.run("cat before-upgrade") == "retained-home"
        fallback = check_state(c)
        assert fallback[0] == first[0] and fallback[1]["task_id"] == first[1]["task_id"] and fallback[2] == first[2]
        assert call(c, "Resource.Get", {"id": volume["resource_id"]})["reference"] == volume
        assert c.run(f"cat {volume_file}") == "retained-volume"
        shutdown(c, admin)

    clean_disk = work / "clean-copy.raw"
    prepare[2] = clean_disk
    clean = json.loads(subprocess.check_output(prepare, timeout=330))
    assert clean["upgrade"]["replayed_ext4_partitions"] == []
    with previous.open("rb") as source, clean_disk.open("rb") as candidate:
        source.seek(65 * 1024 * 1024)
        candidate.seek(source.tell())
        for _ in range(258):
            assert source.read(1024 * 1024) == candidate.read(1024 * 1024)
    assert clean["upgrade"]["verified_authority"]["migration_rehearsal"] == rehearsal

    # Use the disposable, stopped second copy to inject an otherwise valid old schema.
    add_migration_conflict(work, clean_disk)
    conflicting_digest = sha(clean_disk)
    previous_receipt.write_text(json.dumps(clean))
    blocked = work / "must-not-publish.raw"
    prepare[2] = blocked
    prepare[4] = clean_disk
    refused = subprocess.run(prepare, capture_output=True, text=True, timeout=330)
    assert refused.returncode != 0 and "migration-rehearsal-StorageUnavailable" in refused.stderr, refused
    assert not refused.stdout and not blocked.exists()
    assert sha(clean_disk) == conflicting_digest and not list(work.glob(".psios-disk-*"))
    print("PASS clean and crash-replayed UEFI upgrades, private migration rehearsal, publication refusal and independent prior boot")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--kernel-iso", "--previous-archive", "--previous-installer", "--archive", "--installer"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-upgrade-") as directory:
        exercise(args, Path(directory))
