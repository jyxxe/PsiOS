"""Recover a real early native boot failure with no running Core or stored authority."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import uuid

from native_disk import REPO, boot, check_clean_shutdown, extract_partition, password, set_machine_id_fixture, sha


def exercise(args, work):
    _, operator_hash = password(work, "operator.hash")
    disk = work / "native.raw"
    build = json.loads(subprocess.check_output([
        sys.executable, "-B", args.builder, disk, "--kernel-iso", args.kernel_iso,
        "--core-archive", args.archive, "--core-sha256", sha(args.archive), "--console-hash", operator_hash,
    ], timeout=180))
    assert sha(disk) == build["initial_image_sha256"]
    identity = build["state_uuid"]

    def inspect(*options, filesystem=identity, rejection=None, source_digest=None):
        before = sha(disk)
        command = [sys.executable, "-B", args.inspector, disk, "--iso", args.kernel_iso,
                   "--boot-failure-only", "--partition", "2", "--filesystem-uuid", filesystem, *options]
        if source_digest is not None:
            command += ["--source-sha256", source_digest]
        result = subprocess.run(command, capture_output=True, text=True, timeout=240)
        assert sha(disk) == before, "offline rescue changed the source disk"
        if rejection is not None:
            assert result.returncode != 0 and not result.stdout and rejection in result.stderr, result.stderr
            return None
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    assert inspect() == {"filesystem_uuid": identity, "boot_failure": {"status": "Missing"}}
    set_machine_id_fixture(work, disk, "/etc/machine-id", "0" * 32 + "\n", "0100444")
    for source, target in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / target)
    with boot(work, disk) as console:
        console.until(b"PsiOS boot stopped at machine-identity", reject=b"psios login:")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(disk)

    # Independently read the actual record and prove that the failed boot
    # never initialized a Core database.
    state = work / "offline-state.ext4"
    extract_partition(disk, state, 65, 192)
    report = subprocess.run(["debugfs", "-R", "cat /.psios-boot-failure", state],
                            text=True, capture_output=True, check=True, timeout=20)
    failure = json.loads(report.stdout)
    database = subprocess.run(["debugfs", "-R", "stat /psios/state.db", state],
                              capture_output=True, check=True, timeout=20)
    assert b"File not found" in database.stderr
    state.unlink()
    assert failure["stage"] == "machine-identity" and uuid.UUID(failure["boot_id"]).int != 0
    approved = sha(disk)
    assert inspect("--disk-transport", "nvme", source_digest=approved) == {
        "filesystem_uuid": identity, "source_sha256": approved, "boot_failure": {"status": "Found", **failure},
    }
    inspect(filesystem=str(uuid.uuid4()), rejection="filesystem-identity")
    inspect("--journal-overlay-mib", "16", rejection="reads a clean source")
    set_machine_id_fixture(work, disk, "/.psios-boot-failure", '{"status":"Missing"}\n',
                           "0100600", replace=True)
    inspect(rejection="invalid native boot failure record")
    set_machine_id_fixture(work, disk, "/.psios-boot-failure", json.dumps(failure), "0100644", replace=True)
    inspect(rejection="native-boot-failure-record")
    print("PASS offline rescue read the real native boot failure without Core, preserved the source, and rejected invalid evidence")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--builder", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--inspector", type=Path, default=REPO / "images/rescue/inspect.py")
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    with tempfile.TemporaryDirectory(prefix="psios-rescue-native-boot-") as work:
        exercise(parser.parse_args(), Path(work))
