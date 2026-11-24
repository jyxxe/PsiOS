"""Boot the identified native disk with an additional virtio block device attached."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, password, replace_partition, sha


def replace_with_unrelated_partition(work, image, name, start, size):
    partition = work / name
    with partition.open("xb") as handle:
        handle.truncate(size * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", partition], check=True)
    replace_partition(image, partition, start)
    partition.unlink()


def exercise(args, work):
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", work / "code.fd")
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", work / "vars.fd")
    operator, op_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    installed = work / "installed.raw"
    subprocess.run([sys.executable, args.installer, installed, "--kernel-iso", args.kernel_iso,
                    "--core-archive", args.archive, "--core-sha256", sha(args.archive),
                    "--console-hash", op_hash, "--root-console-hash", root_hash],
                   stdout=subprocess.DEVNULL, check=True, timeout=150)
    extra = work / "unrelated.raw"
    with extra.open("xb") as handle:
        handle.write(b"unrelated persistent data\n")
        handle.truncate(installed.stat().st_size)
    extra_digest = sha(extra)
    with boot(work, installed, extra_disk=extra) as c:
        try:
            c.login("psios", operator)
        except AssertionError as failure:
            raise AssertionError(c.buf[-2048:].decode(errors="replace")) from failure
        assert json.loads(c.run("psiosctl whoami"))["principal_id"] == "local-owner"
        c.run('test "$(ls -d /sys/class/block/vd? | wc -l)" -eq 2')
        state = c.run("awk '$2 == \"/var/lib\" { print $1 }' /proc/mounts")
        homes = c.run("awk '$2 == \"/mnt/psios-homes\" { print $1 }' /proc/mounts")
        assert state.endswith("2") and homes == state[:-1] + "3", (state, homes)
        c.send("exit")
        c.login("root", admin)
        c.send("poweroff")
        assert c.vm.wait(35) == 0
    check_clean_shutdown(installed)
    assert sha(extra) == extra_digest

    duplicate = work / "duplicate.raw"
    shutil.copyfile(installed, duplicate)
    original = sha(installed)
    with boot(work, installed, extra_disk=duplicate) as c:
        c.until(b"PsiOS boot stopped at deployment-ambiguity")
        assert c.vm.wait(25) == 0
    assert sha(installed) == sha(duplicate) == original

    replace_with_unrelated_partition(work, installed, "first-home.ext4", 257, 64)
    replace_with_unrelated_partition(work, duplicate, "second-state.ext4", 65, 192)
    before = (sha(installed), sha(duplicate))
    with boot(work, installed, extra_disk=duplicate) as c:
        c.until(b"PsiOS boot stopped at home-device")
        assert c.vm.wait(25) == 0
    assert (sha(installed), sha(duplicate)) == before
    print("Verified a native disk boots beside unrelated storage and preserves both disks when identities duplicate or are split.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-storage-") as root:
        exercise(args, Path(root))
