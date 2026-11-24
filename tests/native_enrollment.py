"""Power-cut native enrollment while preparing and between its two directory publications."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, extract_partition, password, replace_partition, sha


CHECKPOINT = b"PSIOS-ENROLLMENT-POWER-CUT"
PREPARE = "    chown 1000:1000 \"$work\"\n"
PUBLISH = "        publish_authority_directory \"$work/psios\" \"$state\"\n"


def instrument_installer(args, work, hook, partial):
    tree = work / "instrumented"
    native = args.installer.resolve().parent
    shutil.copytree(native, tree / "native")
    shutil.copytree(native.parent / "rescue", tree / "rescue")
    source = tree / "native/enroll"
    script = source.read_text()
    assert script.count(hook) == 1
    injected = partial + f"    sync -f /var/lib\n    printf '%s\\n' '{CHECKPOINT.decode()}' >/dev/console\n    sleep 120\n"
    source.write_text(script.replace(hook, hook + injected))
    return tree / "native/create.py"


def replace_loader(args, work, image, receipt):
    clean = work / "clean-loader"
    clean.mkdir()
    spec = importlib.util.spec_from_file_location("native_installer", args.installer.resolve())
    installer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(installer)
    inputs = argparse.Namespace(kernel_iso=args.kernel_iso, core_archive=args.archive, core_sha256=sha(args.archive))
    installer.ramfs(clean, inputs, receipt["state_uuid"], receipt["home_uuid"])
    efi = installer.boot_media.efi_application(clean, installer.CMDLINE)
    subprocess.run(["mcopy", "-o", "-i", f"{image}@@1048576", efi, "::/EFI/BOOT/BOOTX64.EFI"], check=True)


def recorded_identity(work, image, witness):
    work.mkdir()
    partition = work / "authority.ext4"
    extract_partition(image, partition, 65, 192)
    replay = subprocess.run(["e2fsck", "-p", "-E", "journal_only", partition], stdout=subprocess.DEVNULL)
    assert replay.returncode in (0, 1), replay.returncode
    db = work / "initial.sqlite"
    subprocess.run(["debugfs", "-R", f"dump /psios/state.db {db.name}", partition], cwd=work,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    assert db.exists(), "expected state directory to be published before power loss"
    with sqlite3.connect(db) as conn:
        identity, = conn.execute("SELECT value FROM meta WHERE key='authority'").fetchone()
    token = subprocess.check_output(["debugfs", "-R", f"cat {witness}/owner.token", partition],
                                    stderr=subprocess.DEVNULL)
    assert len(token) == 64
    return identity, hashlib.sha256(token).hexdigest()


def refuse_mixed_markers(work, image, expected):
    partition = work / "conflict.ext4"
    extract_partition(image, partition, 65, 192)
    (work / "empty-marker").touch()
    commands = ("write empty-marker /enrollment-preparing\n"
                "set_inode_field /enrollment-preparing mode 0100444\n"
                "set_inode_field /enrollment-preparing uid 0\n"
                "set_inode_field /enrollment-preparing gid 0\n")
    subprocess.run(["debugfs", "-w", "-f", "-", partition], cwd=work, input=commands, text=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    marker = subprocess.check_output(["debugfs", "-R", "stat /enrollment-preparing", partition],
                                     stderr=subprocess.DEVNULL)
    assert re.search(rb"Type: regular\s+Mode:\s+0444", marker), marker
    assert re.search(rb"User:\s+0\s+Group:\s+0\s+.*Size:\s+0\b", marker), marker
    replace_partition(image, partition, 65)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at authority")
        assert c.vm.wait(35) == 0
    check_clean_shutdown(image)
    assert recorded_identity(work / "retained", image, "/psios.witness") == expected


def exercise(args, work, hook):
    work.mkdir()
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", work / "code.fd")
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", work / "vars.fd")
    operator, op_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    image = work / "installed.raw"
    partial = '    mkdir -m 0700 "$work/psios"\n    printf partial >"$work/psios/state.db"\n' if hook == PREPARE else ""
    variant = instrument_installer(args, work, hook, partial)
    receipt = json.loads(subprocess.check_output([sys.executable, variant, image, "--kernel-iso", args.kernel_iso,
        "--core-archive", args.archive, "--core-sha256", sha(args.archive),
        "--console-hash", op_hash, "--root-console-hash", root_hash], timeout=150))
    with boot(work, image) as c:
        c.until(CHECKPOINT)
        # The context manager kills the VM without a guest shutdown.
    prior = recorded_identity(work / "snapshot", image, "/.core-enrollment/psios.witness") if hook == PUBLISH else None

    replace_loader(args, work, image, receipt)
    with boot(work, image) as c:
        c.login("psios", operator)
        who = json.loads(c.run("psiosctl whoami"))
        assert who["principal_id"] == "local-owner"
        if prior is not None:
            assert who["authority_id"] == prior[0]
            assert c.run("sha256sum /var/lib/psios.witness/owner.token").split()[0] == prior[1]
        c.run("test -f /var/lib/initialization-issued && test ! -e /var/lib/.core-enrollment && "
              "test ! -e /var/lib/enrollment-preparing && test ! -e /var/lib/enrollment-ready")
        c.send("exit")
        c.login("root", admin)
        c.send("poweroff")
        assert c.vm.wait(35) == 0
    check_clean_shutdown(image)
    if prior is not None:
        refuse_mixed_markers(work, image, prior)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-enrollment-") as directory:
        work = Path(directory)
        exercise(args, work / "preparing", PREPARE)
        exercise(args, work / "publishing", PUBLISH)
    print("Verified recovery during first boot, preserved identity after half publication and refusal of conflicting markers.")
