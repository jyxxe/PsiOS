"""Verify and stage the exact factory image for a blank-disk installation."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import uuid

import create


MIB = create.MIB
SECTOR_BYTES = create.SECTOR_BYTES
# The protective MBR can address at most 2^32 sectors including LBA zero.
MAX_TARGET_BYTES = create.MAX_RAW_BYTES
RECEIPT_LIMIT = 16 * 1024
DIRECTORY_LIMIT = 16 * 1024
SHA256 = re.compile(r"[0-9a-f]{64}")
AUTHORITY_PATHS = {
    "state_dir": "/var/lib/psios",
    "witness_dir": "/var/lib/psios.witness",
    "socket": "/run/psios/api.sock",
}


def digest_field(value, name):
    if not isinstance(value, str) or SHA256.fullmatch(value) is None:
        raise ValueError(f"invalid SHA-256 for {name}")
    return value


@contextmanager
def partition_copy(work, image, partition, name):
    start, size, _ = partition
    path = work / name
    scratch = path.open("xb")
    try:
        with scratch, image.open("rb") as source:
            source.seek(start * MIB)
            create.copy_mib(source, scratch, size, replace=False)
            scratch.truncate(size * MIB)
        yield path
    finally:
        path.unlink(missing_ok=True)


def gpt_facts(work, image, layout, *, pass_fds=()):
    return gpt_identity(create.validate_layout(work, image, layout, pass_fds=pass_fds))


def gpt_identity(table):
    disk = uuid.UUID(table["id"])
    identities = {disk}
    partitions = []
    for number, entry in enumerate(table["partitions"], 1):
        identity = uuid.UUID(entry["uuid"])
        if identity.int == 0 or identity in identities:
            raise ValueError("factory partition GUIDs must be distinct and nonzero")
        identities.add(identity)
        partitions.append({
            "number": number,
            "start_sector": entry["start"],
            "sector_count": entry["size"],
            "type_guid": str(uuid.UUID(entry["type"])),
            "partition_uuid": str(identity),
        })
    if disk.int == 0:
        raise ValueError("factory disk GUID must be nonzero")
    return {"disk_uuid": str(disk), "partitions": partitions}


def fit_image(work, image, source, target_size):
    """Relocate GPT metadata on a private verified image; retain every partition extent."""
    original_size = source["size_bytes"]
    if not original_size < target_size <= MAX_TARGET_BYTES or target_size % SECTOR_BYTES:
        raise ValueError("larger targets require whole 512-byte sectors and at most 2 TiB")
    layout = create.Layout(source["geometry_mib"]["state_mib"], source["geometry_mib"]["home_mib"])
    old = create.validate_layout(work, image, layout)
    old_backup = (old["lastlba"] + 1) * SECTOR_BYTES
    tail_bytes = original_size - old_backup
    if not 2 * SECTOR_BYTES <= tail_bytes <= MIB:
        raise ValueError("unsupported GPT backup area on the factory image")
    image.chmod(0o600)
    with image.open("r+b") as disk:
        disk.truncate(target_size)
    relocate = ["sfdisk", "--quiet", "--no-reread", "--no-tell-kernel", "--relocate", "gpt-bak-std", image]
    try:
        create.boot_media.run(work, relocate)
    except subprocess.SubprocessError as error:
        raise ValueError("sfdisk could not relocate the verified GPT to the target capacity") from error
    new_backup = target_size - tail_bytes
    # libfdisk retains its old backup bytes. Clear only those outside the new table.
    with image.open("r+b") as disk:
        disk.seek(old_backup)
        disk.write(bytes(min(original_size, new_backup) - old_backup))
    ready = create.validate_layout(work, image, layout)
    if ((ready["lastlba"] + 1) * SECTOR_BYTES != new_backup or ready["firstlba"] != old["firstlba"]
            or gpt_identity(ready) != {field: source[field] for field in ("disk_uuid", "partitions")}):
        raise ValueError("relocated GPT does not match the verified factory partition layout")
    create.verify_relocated_gpt(image, target_size, ready)
    image.chmod(0o400)
    return {"size_bytes": target_size, "image_sha256": create.sha256(image), "backup_gpt_offset": new_backup}


def check_efi(work, image, expected):
    loader = work / "factory-bootx64.efi"
    with partition_copy(work, image, create.ESP, "factory-esp.fat") as esp:
        subprocess.run(
            ["mcopy", "-i", esp, create.EFI_PATH, loader],
            cwd=work,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            timeout=30,
            check=True,
            preexec_fn=create.file_limit(create.ESP[1] * MIB),
        )
    if create.sha256(loader) != expected:
        raise ValueError("factory EFI application does not match the pinned receipt")
    loader.unlink()


def directory_entries(work, partition, directory):
    with tempfile.TemporaryFile(dir=work) as listing:
        subprocess.run(
            ["debugfs", "-R", f"ls -p {directory}", partition],
            cwd=work,
            stdin=subprocess.DEVNULL,
            stdout=listing,
            stderr=subprocess.DEVNULL,
            timeout=20,
            check=True,
            preexec_fn=create.file_limit(DIRECTORY_LIMIT),
        )
        listing.seek(0)
        rows = listing.read(DIRECTORY_LIMIT + 1).decode("ascii").splitlines()
    entries = {}
    for line in rows:
        if not line:
            continue
        fields = line.split("/")
        if len(fields) != 8 or fields[0] or fields[-1] or fields[5] in entries:
            raise ValueError(f"invalid factory directory entries in {directory}")
        _, inode, mode, uid, gid, name, _, _ = fields
        if not (inode.isdecimal() and mode.isdecimal() and uid.isdecimal() and gid.isdecimal()):
            raise ValueError(f"invalid factory directory metadata in {directory}")
        entries[name] = (int(mode, 8), int(uid), int(gid))
    return entries


def check_ext4(work, partition, expected_uuid, label):
    if uuid.UUID(expected_uuid).int == 0 or create.filesystem_state(partition, expected_uuid) != (1, False):
        raise ValueError(f"factory {label} filesystem must be clean and match the pinned receipt")
    create.boot_media.run(work, ["e2fsck", "-f", "-n", partition])


def check_state(work, state, receipt):
    check_ext4(work, state, receipt["state_uuid"], "state")
    root = (0o040755, 0, 0)
    if directory_entries(work, state, "/") != {
        ".": root, "..": root, "lost+found": (0o040700, 0, 0), "etc": root,
        "deployment-uuid": (0o100444, 0, 0), "initialize-once": (0o100444, 0, 0),
    }:
        raise ValueError("factory state partition is not an uninitialized enrollment seed")
    marker = create.read_root_file(work, state, "/deployment-uuid", 64, "0444")
    if marker != (receipt["state_uuid"] + "\n").encode("ascii"):
        raise ValueError("factory deployment marker does not match the pinned receipt")
    if create.read_root_file(work, state, "/initialize-once", 0, "0444") != b"":
        raise ValueError("factory initialization marker is not empty")
    raw_config = create.read_root_file(work, state, "/etc/psios/core.json", 64 * 1024, "0(?:444|644)")
    config = json.loads(raw_config)
    if not isinstance(config, dict) or any(config.get(name) != value for name, value in AUTHORITY_PATHS.items()):
        raise ValueError("factory Core configuration does not name the native authority paths")

    create.validate_persisted_network(work, state, receipt)
    if "ssh_console" in receipt:
        create.validate_persisted_ssh(work, state)
        ssh_state = directory_entries(work, state, "/etc/psios")
        if {"ssh_host_ed25519_key", ".ssh-host-key.pending"} & ssh_state.keys():
            raise ValueError("factory SSH console already has host key state")
    if "core_limits" in receipt:
        inittab = create.read_root_file(work, state, "/etc/inittab", 16 * 1024, "0(?:600|644)")
        if inittab.splitlines().count(create.LIMITED_SERVICE.encode()) != 1:
            raise ValueError("factory Core service launcher does not apply the recorded resource limits")
    return create.sha256(work / "core.json")


def check_home(work, home, expected_uuid):
    check_ext4(work, home, expected_uuid, "home")
    root = (0o040755, 0, 0)
    root_home = (0o040700, 0, 0)
    operator_home = (0o040700, 1000, 1000)
    if (directory_entries(work, home, "/") != {
            ".": root, "..": root, "lost+found": root_home, "root": root_home, "psios": operator_home,
        } or directory_entries(work, home, "/root") != {".": root_home, "..": root}
            or directory_entries(work, home, "/psios") != {".": operator_home, "..": root}):
        raise ValueError("factory home partition does not contain only the empty native homes")


def stage_image(work: Path, image: Path, receipt: Path, receipt_sha256: str) -> tuple[Path, dict]:
    """Return verified source bytes and authenticated build facts in the private workdir."""
    digest_field(receipt_sha256, "pinned receipt")
    work = work.absolute()
    complete = work / "verified-source.raw"
    with tempfile.TemporaryDirectory(prefix="source-validation-", dir=work) as directory:
        scratch = Path(directory)
        staged_receipt = scratch / "build-receipt.json"
        create.boot_media.pinned_copy(receipt, staged_receipt, receipt_sha256, RECEIPT_LIMIT)
        manifest, layout, _ = create.read_receipt(staged_receipt)
        if (manifest["format"] < 2 or "upgrade" in manifest or "trial_disk_uuid" in manifest
                or layout.raw_disk_bytes is not None):
            raise ValueError("blank installation requires a supported first-install receipt")
        image_sha256 = digest_field(manifest.get("initial_image_sha256"), "factory image")
        deployment_sha256 = digest_field(manifest.get("deployment_manifest_sha256"), "deployment manifest")
        initramfs_sha256 = digest_field(manifest.get("installed_initramfs_sha256"), "installed initramfs")
        size = layout.image_mib * MIB
        staged_image = scratch / "factory.raw"
        create.boot_media.pinned_copy(image, staged_image, image_sha256, size, sparse=True)
        if staged_image.stat().st_size != size:
            raise ValueError("factory image size does not match the pinned receipt")
        gpt = gpt_facts(scratch, staged_image, layout)
        check_efi(scratch, staged_image, manifest["efi_application_sha256"])
        with partition_copy(scratch, staged_image, layout.partitions[1], "factory-state.ext4") as state:
            config_sha256 = check_state(scratch, state, manifest)
        with partition_copy(scratch, staged_image, layout.partitions[2], "factory-home.ext4") as home:
            check_home(scratch, home, manifest["home_uuid"])

        facts = {
            "size_bytes": size,
            "image_sha256": image_sha256,
            "receipt_sha256": receipt_sha256,
            "source_commit": manifest["source_commit"],
            "receipt_format": manifest["format"],
            **gpt,
            "geometry_mib": layout.receipt(),
            "state_uuid": manifest["state_uuid"],
            "home_uuid": manifest["home_uuid"],
            "efi_application_sha256": manifest["efi_application_sha256"],
            "deployment_manifest_sha256": deployment_sha256,
            "installed_initramfs_sha256": initramfs_sha256,
            "core_configuration_sha256": config_sha256,
            "state_initialization": "Fresh",
        }
        for name in ("secure_boot", "network_profile", "wired_driver", "core_limits", "ssh_console", "usb_profile"):
            if name in manifest:
                facts[name] = manifest[name]
        staged_image.chmod(0o400)
        os.link(staged_image, complete)
    return complete, facts
