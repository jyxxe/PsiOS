"""Boot and upgrade a real PsiOS disk with independently sized ext4 partitions."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, check_state, password, sha

MIB = 1024 * 1024
STATE_MIB = 256
HOME_MIB = 96
IMAGE_MIB = 419


def disk_table(disk):
    gpt = json.loads(subprocess.check_output(["sfdisk", "--json", disk], timeout=30))["partitiontable"]
    assert (gpt["label"], gpt["sectorsize"]) == ("gpt", 512), gpt
    partitions = [(p["start"], p["size"], p["type"], p["uuid"]) for p in gpt["partitions"]]
    assert [(start, blocks) for start, blocks, _, _ in partitions] == [
        (2048, 64 * 2048), (65 * 2048, STATE_MIB * 2048), (321 * 2048, HOME_MIB * 2048),
    ], gpt
    return gpt["id"], partitions


def check_capacity(console):
    assert json.loads(console.run("cat /psios-deployment.json"))["geometry_mib"] == {
        "state_mib": STATE_MIB, "home_mib": HOME_MIB, "image_mib": IMAGE_MIB,
    }
    assert console.run("cat /psios-storage-layout").split() == [
        str(mib * 2048) for mib in (65, STATE_MIB, 321, HOME_MIB)]
    assert console.run("cat /sys/class/block/vda2/size /sys/class/block/vda3/size").splitlines() == [
        str(STATE_MIB * 2048), str(HOME_MIB * 2048)]


def exercise(args, work):
    operator, op_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    previous = work / "previous.raw"
    candidate = work / "new.raw"
    receipt = work / "previous.json"
    for source, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / source, work / target)
    build = [sys.executable, args.installer, previous, "--kernel-iso", args.kernel_iso,
             "--core-archive", args.previous_archive, "--core-sha256", sha(args.previous_archive),
             "--console-hash", op_hash, "--root-console-hash", admin_hash]
    for options in (["--state-mib", "191"], ["--home-mib", "8192"]):
        invalid = subprocess.run([*build, *options], capture_output=True, text=True, timeout=30)
        assert invalid.returncode != 0 and "raw image" in invalid.stderr and not previous.exists(), invalid
    raw = subprocess.check_output([*build, "--state-mib", str(STATE_MIB), "--home-mib", str(HOME_MIB)], timeout=180)
    receipt.write_bytes(raw)
    created = json.loads(raw)
    geometry = {"state_mib": STATE_MIB, "home_mib": HOME_MIB, "image_mib": IMAGE_MIB}
    assert created["format"] == 2 and created["geometry_mib"] == geometry, created
    assert previous.stat().st_size == IMAGE_MIB * MIB and previous.stat().st_blocks * 512 < 150 * MIB
    assert sha(previous) == created["initial_image_sha256"]
    partitions = disk_table(previous)

    with boot(work, previous) as console:
        console.login("psios", operator)
        check_capacity(console)
        original = check_state(console)
        console.run("dd if=/dev/zero of=payload bs=1M count=70 2>/dev/null && "
                    "printf 'larger-home\\n' >> payload && sync -f .")
        payload = console.run("sha256sum payload").split()[0]
        assert int(console.run("stat -c '%s' payload")) > 70 * MIB
        console.vm.kill()
        assert console.vm.wait(15) < 0

    before = sha(previous)
    upgrade = [sys.executable, args.installer, candidate, "--upgrade-from", previous,
               "--previous-receipt", receipt, "--kernel-iso", args.kernel_iso,
               "--core-archive", args.archive, "--core-sha256", sha(args.archive)]
    invalid = subprocess.run([*upgrade, "--home-mib", "97"], capture_output=True, text=True, timeout=30)
    assert invalid.returncode != 0 and "partition sizes" in invalid.stderr and not candidate.exists(), invalid
    receipt.write_text(json.dumps(dict(created, geometry_mib=dict(geometry, state_mib=257, home_mib=95))))
    invalid = subprocess.run(upgrade, capture_output=True, text=True, timeout=95)
    assert invalid.returncode != 0 and "GPT layout" in invalid.stderr and not candidate.exists(), invalid
    assert sha(previous) == before
    receipt.write_bytes(raw)

    upgraded = json.loads(subprocess.check_output(upgrade, timeout=330))
    result = upgraded["upgrade"]
    assert upgraded["geometry_mib"] == geometry and disk_table(candidate) == partitions
    assert upgraded["state_uuid"] == created["state_uuid"] and upgraded["home_uuid"] == created["home_uuid"]
    assert result["replayed_ext4_partitions"] == [2, 3] and result["previous_image_sha256"] == before
    assert result["previous_source_commit"] == created["source_commit"] != upgraded["source_commit"]
    verified = result["verified_authority"]
    assert (verified["node_id"], verified["database_format"], verified["migration_required"]) == (
        original[0], args.previous_database_format, True), verified
    assert sha(previous) == before and sha(candidate) == upgraded["initial_image_sha256"]
    with boot(work, candidate) as console:
        console.login("psios", operator)
        check_capacity(console)
        assert console.run("sha256sum payload").split()[0] == payload
        assert console.run("tail -c 12 payload") == "larger-home"
        current = check_state(console)
        assert current[0] == original[0] and current[1]["task_id"] == original[1]["task_id"]
        console.send("exit")
        console.login("root", admin)
        console.send("poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(candidate, starts=(65, 321))
    assert sha(previous) == before
    print("PASS enlarged native state and homes, exact GPT pinning, sparse publication and crash-replayed upgrade")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--kernel-iso", "--archive", "--previous-archive"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--previous-database-format", type=int, default=5)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-capacity-") as directory:
        exercise(args, Path(directory))
