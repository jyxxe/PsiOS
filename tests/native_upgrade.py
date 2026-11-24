"""Copy an installed format 5 UEFI node and boot its independently upgraded disk."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import boot, check_native_network, check_state, password, sha


def shutdown(console, admin):
    console.send("exit")
    console.login("root", admin)
    console.send("cd /; exec poweroff")
    assert console.vm.wait(35) == 0


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
        c.run("echo retained-home > before-upgrade; sync -f .")
        refused = subprocess.run(prepare, capture_output=True, text=True, timeout=95)
        assert refused.returncode != 0 and "lock" in refused.stderr and not next_disk.exists(), refused
        c.send("exit")
        c.login("root", admin)
        c.run("sed -i 's/^{/{\"workers\":2,/' /etc/psios/core.json")
        c.run("echo retained-admin > /root/maintenance; sync")
        # Capture a crash-consistent image with real outstanding ext4 journals.
        c.vm.kill()
        assert c.vm.wait(15) < 0

    old_digest = sha(previous)
    previous_receipt.write_text(json.dumps(dict(old, efi_application_sha256="0" * 64)))
    refused = subprocess.run(prepare, capture_output=True, text=True, timeout=95)
    assert refused.returncode != 0 and "EFI application" in refused.stderr and not next_disk.exists(), refused
    previous_receipt.write_text(json.dumps(dict(old, home_uuid="00000000-0000-4000-8000-000000000001")))
    refused = subprocess.run(prepare, capture_output=True, text=True, timeout=95)
    assert refused.returncode != 0 and "previous deployment UUID" in refused.stderr and not next_disk.exists(), refused
    assert sha(previous) == old_digest
    previous_receipt.write_bytes(build)

    upgrade = json.loads(subprocess.check_output(prepare, timeout=330))
    verified = upgrade["upgrade"]["verified_authority"]
    assert (verified["node_id"], verified["database_format"], verified["migration_required"]) == (first[0], 5, True), verified
    assert upgrade["state_uuid"] == old["state_uuid"] and upgrade["home_uuid"] == old["home_uuid"]
    assert upgrade["upgrade"]["previous_image_sha256"] == sha(previous) == old_digest
    assert upgrade["upgrade"]["previous_source_commit"] == old["source_commit"] != upgrade["source_commit"]
    assert upgrade["initial_image_sha256"] == sha(next_disk)
    assert next_disk.stat().st_mode & 0o077 == 0
    assert upgrade["upgrade"]["replayed_ext4_partitions"] == [2, 3]

    with boot(work, next_disk) as c:
        c.login("psios", operator)
        assert c.run("sha256sum /psios-deployment.json").split()[0] == upgrade["deployment_manifest_sha256"]
        assert c.run("cat /psios-source") == upgrade["source_commit"]
        after = check_state(c)
        assert after[0] == first[0] and after[1]["task_id"] == first[1]["task_id"] and after[2] == first[2]
        assert after[1]["deduplicated"] and after[3] != first[3]
        assert c.run("cat before-upgrade") == "retained-home"
        assert json.loads(c.run("psiosctl node"))["state"]["configuration"]["worker_count"] == 2
        check_native_network(c)
        c.send("exit")
        c.login("root", admin)
        assert c.run("cat maintenance") == "retained-admin"
        c.send("poweroff")
        assert c.vm.wait(35) == 0

    assert sha(previous) == old_digest
    with boot(work, previous) as c:
        c.login("psios", operator)
        assert c.run("cat /psios-source") == old["source_commit"]
        assert c.run("cat before-upgrade") == "retained-home"
        fallback = check_state(c)
        assert fallback[0] == first[0] and fallback[1]["task_id"] == first[1]["task_id"] and fallback[2] == first[2]
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
    print("PASS clean and crash-replayed UEFI upgrades, receipt and lock checks, preserved authority and homes, and independent prior boot")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--kernel-iso", "--previous-archive", "--previous-installer", "--archive", "--installer"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-upgrade-") as directory:
        exercise(args, Path(directory))
