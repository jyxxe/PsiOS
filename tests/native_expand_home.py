"""Grow a stopped native PsiOS home partition and boot both deployments."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, call, check_clean_shutdown, check_state, password, sha

MIB = 1024 * 1024
HOME_START_MIB = 257
OLD_HOME_MIB = 64
NEW_HOME_MIB = 192
NEW_DISK_MIB = HOME_START_MIB + NEW_HOME_MIB + 2
PREALLOCATED_BYTES = 512 * MIB + 512


def read_gpt(disk):
    gpt = json.loads(subprocess.check_output(["sfdisk", "--json", disk], timeout=30))["partitiontable"]
    return gpt["id"], [{k: v for k, v in part.items() if k != "node"} for part in gpt["partitions"]]


def write_at(disk, offset, data):
    with disk.open("r+b") as image:
        image.seek(offset)
        image.write(data)


def poweroff(console, root_password):
    console.send("exit")
    console.login("root", root_password)
    console.send("poweroff")
    assert console.vm.wait(35) == 0


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    for name in ("CODE", "VARS"):
        shutil.copyfile(args.ovmf_dir / f"OVMF_{name}_4M.fd", work / f"{name.lower()}.fd")
    source, candidate = work / "source.raw", work / "expanded.raw"
    receipt = work / "source.json"
    builder = [sys.executable, args.installer]
    boot_inputs = ["--kernel-iso", args.kernel_iso, "--core-archive", args.archive,
                   "--core-sha256", sha(args.archive)]
    factory = [*builder, source, *boot_inputs, "--console-hash", operator_hash, "--root-console-hash", root_hash]
    invalid = subprocess.run([*factory, "--expand-home-mib", str(NEW_HOME_MIB)], capture_output=True, text=True, timeout=20)
    assert invalid.returncode != 0 and "--expand-home-mib requires --upgrade-from" in invalid.stderr
    assert not source.exists()
    receipt.write_bytes(subprocess.check_output(factory, timeout=180))
    created = json.loads(receipt.read_bytes())

    with boot(work, source) as console:
        console.login("psios", operator)
        prior = check_state(console)
        console.run("umask 077; printf 'original home payload\\n' > old-home; sync -f .")
        file_digest = console.run("sha256sum old-home").split()[0]
        # Give the candidate a genuine ext4 journal to replay.
        console.vm.kill()
        assert console.vm.wait(15) < 0

    capacity = []
    retained_offset = (HOME_START_MIB + NEW_HOME_MIB + 1) * MIB
    sentinel = b"Bytes beyond the enlarged home."
    if args.preallocated:
        original = source.stat().st_size
        original_gpt = read_gpt(source)
        with source.open("r+b") as image:
            image.truncate(PREALLOCATED_BYTES)
        subprocess.run(["sfdisk", "--quiet", "--no-reread", "--no-tell-kernel",
                        "--relocate", "gpt-bak-std", source], check=True, timeout=30)
        # A factory installer leaves no stale backup at its old end.
        write_at(source, original - 33 * 512, bytes(33 * 512))
        assert read_gpt(source) == original_gpt
        write_at(source, retained_offset, sentinel)
        capacity = ["--source-size-bytes", str(PREALLOCATED_BYTES)]

    before = sha(source)
    disk_id, partitions = read_gpt(source)
    upgrade = [*builder, candidate, *boot_inputs, "--upgrade-from", source, "--previous-receipt", receipt, *capacity]
    for size in (OLD_HOME_MIB - 1, OLD_HOME_MIB, 8192):
        invalid = subprocess.run([*upgrade, "--expand-home-mib", str(size)], capture_output=True, text=True, timeout=30)
        assert invalid.returncode != 0 and not candidate.exists()
        assert "home" in invalid.stderr, invalid.stderr
    start = (HOME_START_MIB + OLD_HOME_MIB) * MIB
    write_at(source, start, b"occupied")
    occupied = sha(source)
    expanded = [*upgrade, "--expand-home-mib", str(NEW_HOME_MIB)]
    rejected = subprocess.run(expanded, capture_output=True, text=True, timeout=120)
    assert rejected.returncode != 0 and "contains data outside its original partition" in rejected.stderr, rejected.stderr
    assert not candidate.exists() and sha(source) == occupied
    write_at(source, start, bytes(len(b"occupied")))
    assert sha(source) == before

    result = json.loads(subprocess.check_output(expanded, timeout=330))
    assert result["geometry_mib"] == {"state_mib": 192, "home_mib": NEW_HOME_MIB, "image_mib": NEW_DISK_MIB}
    assert result["upgrade"]["home_expansion"] == {"previous_mib": OLD_HOME_MIB, "target_mib": NEW_HOME_MIB}
    assert result["upgrade"]["previous_image_sha256"] == before and sha(source) == before
    assert result["initial_image_sha256"] == sha(candidate)
    assert [result[key] for key in ("state_uuid", "home_uuid")] == [created[key] for key in ("state_uuid", "home_uuid")]
    assert result["upgrade"]["verified_authority"]["node_id"] == prior[0]
    assert result["upgrade"]["replayed_ext4_partitions"] == [2, 3]
    partitions[2]["size"] = NEW_HOME_MIB * 2048
    assert read_gpt(candidate) == (disk_id, partitions)
    if args.preallocated:
        assert candidate.stat().st_size == result["raw_disk_bytes"] == PREALLOCATED_BYTES
        with candidate.open("rb") as image:
            image.seek(retained_offset)
            assert image.read(len(sentinel)) == sentinel
    else:
        assert candidate.stat().st_size == NEW_DISK_MIB * MIB and "raw_disk_bytes" not in result

    with boot(work, candidate) as console:
        console.login("psios", operator)
        assert json.loads(console.run("cat /psios-deployment.json"))["geometry_mib"] == result["geometry_mib"]
        assert int(console.run("cat /sys/class/block/vda3/size")) == NEW_HOME_MIB * 2048
        assert console.run("sha256sum old-home").split()[0] == file_digest
        assert console.run("stat -c '%u:%g:%a' old-home") == "1000:1000:600"
        assert call(console, "Task.Wait", {"id": prior[1]["task_id"], "timeout_ms": 5000})["result"] == prior[2]
        console.run("dd if=/dev/zero of=new-home-data bs=1M count=110 2>/dev/null; test $(stat -c %s new-home-data) = 115343360")
        poweroff(console, root)
    check_clean_shutdown(candidate)
    assert sha(source) == before
    with boot(work, source) as console:
        console.login("psios", operator)
        assert int(console.run("cat /sys/class/block/vda3/size")) == OLD_HOME_MIB * 2048
        console.run("test ! -e new-home-data")
        assert console.run("sha256sum old-home").split()[0] == file_digest
        assert call(console, "Task.Wait", {"id": prior[1]["task_id"], "timeout_ms": 5000})["result"] == prior[2]
        poweroff(console, root)
    check_clean_shutdown(source)
    path = "within existing disk capacity" if args.preallocated else "with a larger raw image"
    print(f"PASS expanded the native ext4 home {path}, retained original Task and booted both disks")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for item in ("kernel-iso", "archive"):
        parser.add_argument(f"--{item}", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    parser.add_argument("--preallocated", action="store_true", help="use the unused tail of an already larger disk")
    with tempfile.TemporaryDirectory(prefix="psios-native-expand-home-") as directory:
        exercise(parser.parse_args(), Path(directory))
