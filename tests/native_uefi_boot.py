"""Compare native startup evidence with the actual read-only UEFI variables."""

import argparse
import json
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile

from native_disk import boot, password, sha
from native_install_virtio import BOOT_ASSET_DIRS, extract_tools


EFI_GLOBAL = "8be4df61-93ca-11d2-aa0d-00e098032b8c"


def build(args, work, name, *options):
    image = work / f"{name}.raw"
    receipt = work / f"{name}.json"
    with receipt.open("xb") as output:
        subprocess.run([sys.executable, "-B", work / "native/create.py", image,
                        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive,
                        "--core-sha256", args.archive_sha256, *options], stdout=output, check=True, timeout=240)
    assert json.loads(receipt.read_bytes())["initial_image_sha256"] == sha(image)
    return image, receipt


def status(console):
    return json.loads(console.run("psiosctl native status"))


def read_firmware(console):
    mount = "/sys/firmware/efi/efivars"
    console.run(f"mount -t efivarfs -o ro,nosuid,nodev,noexec efivarfs {mount}")
    try:
        def content(name):
            raw = console.run(f"od -An -v -tx1 -N133 {mount}/{name}-{EFI_GLOBAL}")
            data = bytes.fromhex(raw)
            assert 6 <= len(data) <= 132 and len(data) % 2 == 0, data.hex()
            return struct.unpack_from(f"<{(len(data) - 4) // 2}H", data, 4)

        current, = content("BootCurrent")
        return {"status": "Observed", "current": current, "order": list(content("BootOrder"))}
    finally:
        console.run(f"umount {mount}")


def sample(work, image, operator, admin, *, enabled, damage_record=False):
    with boot(work, image) as console:
        console.login("psios", operator)
        observation = status(console)
        evidence = observation["uefi_boot_entries"]
        assert observation["firmware"] == "UEFI" and observation["core_socket"] == "Present"
        assert observation["secure_boot_observation"] == "Unmeasured"
        result = json.loads(console.run("printf '{}' | psiosctl submit system.status --key uefi-evidence"))
        task = json.loads(console.run(f"psiosctl wait {result['task_id']}"))
        assert task["lifecycle"] == "Succeeded"
        identity = json.loads(console.run("psiosctl whoami"))["authority_id"]
        console.send("exit")
        console.login("root", admin)
        console.run("! grep -q ' - efivarfs ' /proc/self/mountinfo && ! ls -d /run/.psios-efivars.* >/dev/null 2>&1")
        if enabled:
            assert evidence == read_firmware(console), evidence
        else:
            assert evidence == {"status": "NotRequested"}, evidence
        if damage_record:
            console.run("mv /run/psios-boot.json /run/psios-boot-test.json")
            assert status(console)["uefi_boot_entries"] == {"status": "Unavailable"}
            console.run(r"""sed 's/"order":\[[0-9,]*\]/"order":[7,7]/' /run/psios-boot-test.json """
                        "> /run/psios-boot.json && chmod 0444 /run/psios-boot.json")
            assert status(console)["uefi_boot_entries"] == {"status": "Invalid"}
            assert status(console)["secure_boot_observation"] == "Unmeasured"
            console.run("mv -f /run/psios-boot-test.json /run/psios-boot.json")
            assert status(console)["uefi_boot_entries"] == evidence
        console.send("poweroff")
        assert console.vm.wait(35) == 0
    return identity, result, observation["boot_id"], evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--archive-sha256", required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    if not args.archive.is_file() or sha(args.archive) != args.archive_sha256:
        parser.error("portable archive differs from its expected SHA-256")
    with tempfile.TemporaryDirectory(prefix="psios-native-uefi-entries-") as directory:
        work = Path(directory)
        for folder in BOOT_ASSET_DIRS:
            (work / folder).mkdir()
        extract_tools(args.archive, work)
        for original, installed in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
            shutil.copyfile(args.ovmf_dir / original, work / installed)
        operator, operator_hash = password(work, "operator.hash")
        admin, admin_hash = password(work, "admin.hash")
        console_keys = ("--console-hash", operator_hash, "--root-console-hash", admin_hash)
        ordinary, _ = build(args, work, "ordinary", *console_keys)
        sample(work, ordinary, operator, admin, enabled=False)
        ordinary.unlink()
        enrolled, receipt = build(args, work, "entries", *console_keys, "--uefi-boot-entries")
        assert json.loads(receipt.read_bytes())["uefi_boot_entries"] is True
        who, task, boot_id, entries = sample(work, enrolled, operator, admin, enabled=True, damage_record=True)
        source = sha(enrolled)
        candidate, candidate_receipt = build(args, work, "upgraded", "--upgrade-from", enrolled,
                                             "--previous-receipt", receipt)
        assert sha(enrolled) == source and json.loads(candidate_receipt.read_bytes())["uefi_boot_entries"] is True
        same, replay, next_boot, next_entries = sample(work, candidate, operator, admin, enabled=True)
        assert who == same and next_boot != boot_id and replay["deduplicated"] and replay["task_id"] == task["task_id"]
        print(f"PASS exact UEFI BootCurrent and BootOrder on a native disk and its offline upgrade {entries} {next_entries}")


if __name__ == "__main__":
    main()
