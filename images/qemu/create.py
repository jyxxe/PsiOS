#!/usr/bin/env python3
"""Prepare one unbooted, standalone amd64 guest disk and its private NoCloud seed."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import uuid


HERE = Path(__file__).resolve().parent


def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def create(base_path, package, key_path, output):
    base = json.loads((HERE / "base.json").read_text())
    if not base_path.is_file() or base_path.stat().st_size != base["size_bytes"]:
        raise ValueError("unexpected base image size")
    if not package.is_file() or not 0 < package.stat().st_size <= 128 * 1024 * 1024:
        raise ValueError("expected a local Core package within 128 MiB")
    if not key_path.is_file() or key_path.stat().st_size > 16 * 1024:
        raise ValueError("expected an OpenSSH public key file")
    pubkey = key_path.read_text().strip()
    if "\n" in pubkey or not pubkey.startswith(("ssh-ed25519 ", "ssh-rsa ", "ecdsa-sha2-")):
        raise ValueError("expected one OpenSSH public key")
    subprocess.run(["ssh-keygen", "-lf", key_path], stdout=subprocess.DEVNULL, check=True)
    output.mkdir(mode=0o700)
    try:
        disk = output / "system.qcow2"
        shutil.copyfile(base_path, disk)
        if digest(disk) != base["sha256"]:
            raise ValueError("base image differs from the pinned release")
        info = json.loads(subprocess.check_output(["qemu-img", "info", "-f", "qcow2", "--output=json", disk]))
        if info.get("backing-filename"):
            raise ValueError("base image must be standalone")
        subprocess.run(["qemu-img", "resize", "-f", "qcow2", disk, "8G"], check=True)
        with tempfile.TemporaryDirectory(dir=output) as stage:
            seed = Path(stage)
            deb = seed / "psios-core.deb"
            shutil.copyfile(package, deb)
            fields = subprocess.check_output(["dpkg-deb", "-W", "--showformat",
                "${Package} ${Architecture} ${Version}", deb], text=True).split()
            if len(fields) != 3 or fields[:2] != ["psios-core", "amd64"]:
                raise ValueError("expected the amd64 psios-core binary package")
            checksum = digest(deb)
            instance_id = "psios-" + str(uuid.uuid4())
            # JSON is valid YAML. Key comments are data and never reach a shell command.
            config = {
                "users": [{"name": "psios-admin", "shell": "/bin/bash", "lock_passwd": True,
                           "sudo": ["ALL=(ALL) NOPASSWD:ALL"], "ssh_authorized_keys": [pubkey]}],
                "ssh_pwauth": False, "disable_root": True, "package_update": False, "package_upgrade": False,
                "write_files": [
                    {"path": "/usr/local/libexec/psios-provision", "permissions": "0700",
                     "content": (HERE / "provision").read_text()},
                    {"path": "/etc/systemd/system/psios-provision.service", "permissions": "0644",
                     "content": (HERE / "psios-provision.service").read_text()},
                    {"path": "/etc/psios-provision.sha256", "permissions": "0600",
                     "content": checksum + "  psios-core.deb\n"},
                ],
                "runcmd": [["systemctl", "daemon-reload"], ["systemctl", "enable", "--now", "psios-provision.service"]],
            }
            (seed / "user-data").write_text("#cloud-config\n" + json.dumps(config) + "\n")
            (seed / "meta-data").write_text(json.dumps({"instance-id": instance_id, "local-hostname": "psios"}) + "\n")
            subprocess.run(["genisoimage", "-quiet", "-volid", "cidata", "-joliet", "-rock",
                            "-output", (output / "seed.iso").absolute(), "."], cwd=seed, check=True)
        # This manifest marks a fully prepared, unbooted instance. Disk contents
        # and its digest naturally change after the operator starts the guest.
        manifest = {"architecture": "amd64", "firmware": "SeaBIOS", "base": base,
                    "instance_id": instance_id, "package_version": fields[2], "package_sha256": checksum,
                    "disk_sha256": digest(disk), "seed_sha256": digest(output / "seed.iso")}
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    except BaseException:
        shutil.rmtree(output)
        raise


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--authorized-key", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="new directory")
    args = parser.parse_args()
    os.umask(0o077)
    create(args.base, args.package, args.authorized_key, args.output)
