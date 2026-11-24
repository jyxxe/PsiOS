"""Boot an enrolled UEFI firmware with a signed native UKI and reject altered boot media."""
import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

import pefile

from native_disk import Console, REPO, password, sha

EFI_FILE = "::/EFI/BOOT/BOOTX64.EFI"


@contextmanager
def secure_vm(work, image):
    shutil.copyfile(work / "enrolled.fd", work / "vars.fd")
    cmd = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-machine", "q35,smm=on",
           "-accel", "tcg", "-m", "768", "-smp", "2", "-display", "none", "-monitor", "none", "-serial", "stdio",
           "-global", "driver=cfi.pflash01,property=secure,value=on", "-nic", "none",
           "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=code.fd",
           "-drive", "if=pflash,format=raw,unit=1,file=vars.fd",
           "-drive", f"if=virtio,format=raw,file={image.name}", "-boot", "order=c,strict=on"]
    with subprocess.Popen(cmd, cwd=work, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as vm:
        try:
            yield Console(vm)
        finally:
            if vm.poll() is None:
                vm.kill()
            vm.wait(15)


def create_signer(work, name):
    key, cert = work / (name + ".key"), work / (name + ".pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-nodes", "-batch",
                    "-subj", f"/CN=PsiOS {name} acceptance/", "-keyout", key, "-out", cert, "-days", "1"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    key.chmod(0o600)
    return key, cert


def check_firmware(c):
    available = c.run("grep -qw efivarfs /proc/filesystems || modprobe efivarfs 2>/dev/null; "
                      "if grep -qw efivarfs /proc/filesystems; then echo available; fi")
    if available != "available":
        state = c.run("dmesg | grep -Ei 'secure.boot|lockdown' || true")
        assert "secure boot enabled" in state.lower() or "locked down from EFI Secure Boot" in state, state
        return
    c.run("grep -qw efivarfs /proc/mounts || mount -t efivarfs efivarfs /sys/firmware/efi/efivars")
    for label, expected in [("SecureBoot", "1"), ("SetupMode", "0")]:
        marker = c.run(f"od -An -j 4 -N 1 -t u1 /sys/firmware/efi/efivars/{label}-*")
        assert marker == expected, (label, marker)


def rejected_by_firmware(c):
    seen = b""
    until = time.monotonic() + 45
    while (remaining := until - time.monotonic()) > 0 and select.select([c.vm.stdout], [], [], remaining)[0]:
        chunk = os.read(c.vm.stdout.fileno(), 8192)
        if not chunk:
            break
        seen = (seen + chunk)[-32768:]
        if b"psios login:" in seen or b"psiosd ready" in seen:
            raise AssertionError("firmware ran the modified signed image")
        if b"Security Violation" in seen or b"Access Denied" in seen:
            return
    raise AssertionError(f"OVMF did not report signature rejection: {seen[-1024:]!r}")


def installer(args, image, archive, program=None):
    return [sys.executable, program or args.installer, image, "--kernel-iso", args.kernel_iso,
            "--core-archive", archive, "--core-sha256", sha(archive)]


def signing_args(args, key, cert):
    return ["--secure-boot-key", key, "--secure-boot-cert", cert,
            "--efi-stub", args.efi_stub, "--efi-stub-sha256", sha(args.efi_stub)]


def exercise(args, work):
    operator, op_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    key, cert = create_signer(work, "enrolled")
    owner = str(uuid.uuid4())
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.secboot.fd", work / "code.fd")
    subprocess.run(["virt-fw-vars", "-i", args.ovmf_dir / "OVMF_VARS_4M.fd", "-o", work / "enrolled.fd",
                    "--set-pk", owner, cert, "--add-kek", owner, cert, "--add-db", owner, cert, "--sb"], check=True)
    image = work / "signed.raw"
    security = signing_args(args, key, cert)
    receipt = json.loads(subprocess.check_output([*installer(args, image, args.previous_archive),
                                                  "--console-hash", op_hash, "--root-console-hash", root_hash,
                                                  *security]))
    assert receipt["format"] == 3 and receipt["initial_image_sha256"] == sha(image), receipt
    assert receipt["secure_boot"]["efi_stub_sha256"] == sha(args.efi_stub), receipt
    with secure_vm(work, image) as c:
        c.login("root", admin)
        check_firmware(c)
        c.send("exit")
        c.login("psios", operator)
        manifest = json.loads(c.run("cat /psios-deployment.json"))
        assert manifest["secure_boot"] == receipt["secure_boot"], manifest
        assert c.run("sha256sum /psios-deployment.json").split()[0] == receipt["deployment_manifest_sha256"]
        identity = json.loads(c.run("psiosctl whoami"))["authority_id"]
        c.send("exit")
        c.login("root", admin)
        c.send("poweroff")
        assert c.vm.wait(35) == 0

    # Change bytes within the authenticated initrd of a disposable disk copy.
    signed = work / "original.efi"
    subprocess.run(["mcopy", "-i", f"{image}@@1048576", EFI_FILE, signed], check=True)
    assert sha(signed) == receipt["efi_application_sha256"]
    pe = pefile.PE(str(signed), fast_load=True)
    initrd, = [s for s in pe.sections if s.Name.rstrip(b"\x00") == b".initrd"]
    content = bytearray(signed.read_bytes())
    content[initrd.PointerToRawData + 32] ^= 1
    altered = work / "altered.efi"
    altered.write_bytes(content)
    damaged = work / "damaged.raw"
    subprocess.run(["qemu-img", "convert", "-m", "1", "-f", "raw", "-O", "raw", image, damaged], check=True)
    subprocess.run(["mcopy", "-o", "-i", f"{damaged}@@1048576", altered, EFI_FILE], check=True)
    damaged_digest = sha(damaged)
    with secure_vm(work, damaged) as c:
        rejected_by_firmware(c)
    assert sha(damaged) == damaged_digest
    damaged.unlink()

    before = sha(image)
    receipt_path = work / "previous.json"
    receipt_path.write_text(json.dumps(receipt))
    upgraded = work / "upgraded.raw"
    upgrade = ["--upgrade-from", image, "--previous-receipt", receipt_path]
    base = installer(args, upgraded, args.archive)
    other_key, other_cert = create_signer(work, "untrusted")
    for invalid in ([], signing_args(args, other_key, other_cert)):
        result = subprocess.run([*base, *upgrade, *invalid], capture_output=True, text=True)
        assert result.returncode and "requires the existing signing certificate" in result.stderr, result
        assert not upgraded.exists() and sha(image) == before
    older = subprocess.run([*installer(args, upgraded, args.archive, args.previous_installer), *upgrade],
                           capture_output=True, text=True)
    assert older.returncode and "unsupported previous build receipt format" in older.stderr, older
    assert not upgraded.exists() and sha(image) == before
    new = json.loads(subprocess.check_output([*base, *upgrade, *security]))
    assert new["format"] == 3 and new["secure_boot"] == receipt["secure_boot"], new
    assert new["source_commit"] != receipt["source_commit"], new
    assert new["state_uuid"] == receipt["state_uuid"] and new["home_uuid"] == receipt["home_uuid"], new
    assert new["upgrade"]["verified_authority"]["node_id"] == identity and sha(image) == before, new
    with secure_vm(work, upgraded) as c:
        c.login("root", admin)
        check_firmware(c)
        c.send("exit")
        c.login("psios", operator)
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == identity
        assert json.loads(c.run("cat /psios-deployment.json"))["core_source_commit"] == new["source_commit"]
    print(f"PASS enforced UEFI Secure Boot, rejection of an altered initrd and signed upgrade of authority {identity}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", default=REPO / "images/native/create.py", type=Path)
    parser.add_argument("--ovmf-dir", default=Path("/usr/share/OVMF"), type=Path)
    for arg in ("--efi-stub", "--kernel-iso", "--archive", "--previous-archive", "--previous-installer"):
        parser.add_argument(arg, required=True, type=Path)
    with tempfile.TemporaryDirectory(prefix="psios-secure-boot-") as directory:
        exercise(parser.parse_args(), Path(directory))
