"""Pinned kernel inputs, UEFI loaders and read-only El Torito rescue media."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
from typing import NamedTuple


EPOCH = 946684800  # 2000-01-01 UTC, also representable by FAT and ISO 9660.
CMDLINE = "console=ttyS0,115200 quiet panic=1 scsi_mod.scan=sync rdinit=/psios-rescue"
MIB = 1024 * 1024
SECURE_BOOT_SCHEME = "uefi-uki-authenticode"


def regular_file(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError(f"expected a regular file: {path}")
    return os.fdopen(fd, "rb")


def pinned_copy(source, target, expected, limit, *, sparse=False):
    with regular_file(source) as src, target.open("xb") as dst:
        copy_pinned(src, dst, source, expected, limit, sparse=sparse)


def copy_pinned(src, dst, source, expected, limit, *, sparse=False):
    digest = hashlib.sha256()
    zero = bytes(MIB) if sparse else None
    while chunk := src.read(MIB):
        limit -= len(chunk)
        if limit < 0:
            raise ValueError(f"input too large: {source}")
        digest.update(chunk)
        if zero is not None and chunk == zero[:len(chunk)]:
            dst.seek(len(chunk), os.SEEK_CUR)
        else:
            dst.write(chunk)
    if sparse:
        dst.truncate()
    if digest.hexdigest() != expected:
        raise ValueError(f"SHA-256 mismatch: {source}")


def prepare_kernel(work, kernel_iso):
    base = json.loads((Path(__file__).resolve().with_name("base.json")).read_text())
    iso = work / "base.iso"
    pinned_copy(kernel_iso, iso, base["sha256"], base["size_bytes"])
    for name, member, digest in [("kernel", "vmlinuz-virt", "kernel_sha256"),
                                 ("initrd", "initramfs-virt", "initramfs_sha256")]:
        path = work / name
        with path.open("wb") as out:
            subprocess.run(["isoinfo", "-R", "-i", iso, "-x", "/boot/" + member], stdout=out, check=True, timeout=30)
        with path.open("rb") as f:
            if hashlib.file_digest(f, "sha256").hexdigest() != base[digest]:
                raise ValueError(f"invalid {member} in boot media")
    return base


def run(work, args, *, pass_fds=()):
    env = {**os.environ, "LC_ALL": "C", "TZ": "UTC", "SOURCE_DATE_EPOCH": str(EPOCH)}
    subprocess.run(args, cwd=work, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                   pass_fds=pass_fds, timeout=90, check=True)


def efi_application(work, cmdline):
    # Keep the UEFI loader, kernel and initrd together. GRUB boots from its own
    # memory disk without searching the subject volume for configuration or code.
    grub = work / "grub.cfg"
    grub.write_text(f"linux (memdisk)/boot/kernel {cmdline}\ninitrd (memdisk)/boot/initrd\nboot\n")
    for source in (grub, work / "kernel", work / "initrd"):
        os.utime(source, (EPOCH, EPOCH))
    efi = work / "BOOTX64.EFI"
    modules = "linux normal memdisk tar"
    run(work, ["grub-mkstandalone", "--format=x86_64-efi", "--modules=" + modules, "--install-modules=" + modules,
         "--locales=", "--fonts=", "--themes=", "--output", efi, "boot/grub/grub.cfg=grub.cfg",
         "boot/kernel=kernel", "boot/initrd=initrd"])
    os.utime(efi, (EPOCH, EPOCH))
    return efi


class SecureBoot(NamedTuple):
    key: Path
    certificate: Path
    stub: Path
    certificate_sha256: str
    stub_sha256: str

    def receipt(self):
        return {"scheme": SECURE_BOOT_SCHEME, "certificate_sha256": self.certificate_sha256,
                "efi_stub_sha256": self.stub_sha256}

    def verify(self, work, image):
        run(work, ["sbverify", "--cert", self.certificate, image])


def add_signing_arguments(parser):
    parser.add_argument("--secure-boot-key", type=Path, help="private local PEM key for an enrolled UEFI signing certificate")
    parser.add_argument("--secure-boot-cert", type=Path, help="PEM signing certificate trusted by the target UEFI firmware")
    parser.add_argument("--efi-stub", type=Path, help="trusted systemd linuxx64.efi.stub input for a signed Unified Kernel Image")
    parser.add_argument("--efi-stub-sha256", help="SHA-256 of the exact trusted EFI stub input")


def signing_requested(args, parser):
    inputs = (args.secure_boot_key, args.secure_boot_cert, args.efi_stub, args.efi_stub_sha256)
    if all(value is None for value in inputs):
        return False
    if not all(inputs) or not re.fullmatch(r"[0-9a-f]{64}", args.efi_stub_sha256):
        parser.error("Secure Boot requires the key, certificate, EFI stub and its lowercase SHA-256")
    return True


def prepare_secure_boot(work, key, certificate, stub, stub_digest):
    pinned_stub = work / "linuxx64.efi.stub"
    pinned_copy(stub, pinned_stub, stub_digest, 2 * MIB)
    with regular_file(certificate) as source:
        pem = source.read(64 * 1024 + 1)
        if len(pem) > 64 * 1024:
            raise ValueError("Secure Boot certificate exceeds size limit")
    trusted = work / "uki-signer.pem"
    trusted.write_bytes(pem)
    der = subprocess.check_output(["openssl", "x509", "-in", trusted, "-outform", "DER"],
                                  stdin=subprocess.DEVNULL, timeout=10)
    return SecureBoot(key, trusted, pinned_stub, hashlib.sha256(der).hexdigest(), stub_digest)


def secure_efi_application(work, cmdline, signing):
    unsigned = work / "unsigned.efi"
    efi = work / "BOOTX64.EFI"
    run(work, ["ukify", "build", "--config=/dev/null", "--efi-arch=x64", "--stub", signing.stub,
               "--linux", work / "kernel", "--initrd", work / "initrd", "--cmdline", cmdline,
               "--os-release=ID=psios\nNAME=PsiOS\n", "--output", unsigned])
    with regular_file(signing.key) as key:
        info = os.fstat(key.fileno())
        if info.st_uid != os.geteuid() or info.st_mode & 0o077 or not 0 < info.st_size <= 64 * 1024:
            raise ValueError("Secure Boot key must be a private regular file owned by the builder")
        run(work, ["sbsign", "--key", f"/proc/self/fd/{key.fileno()}", "--cert", signing.certificate,
                   "--output", efi, unsigned], pass_fds=(key.fileno(),))
    signing.verify(work, efi)
    os.utime(efi, (EPOCH, EPOCH))
    return efi


def bios_loader(work, tree):
    bios = tree / "boot/syslinux"
    bios.mkdir()
    for name, digest in (
        ("isolinux.bin", "d27cf552dcd95651447178bc2bdd084b3f821911bf68b95502735b1acab83941"),
        ("ldlinux.c32", "6bb315e3a4def84f4ae55cda933e4848fb28bb0745282cf8f86bb9283085590b"),
    ):
        path = bios / name
        with path.open("xb") as output:
            subprocess.run(["isoinfo", "-R", "-i", work / "base.iso", "-x", "/boot/syslinux/" + name],
                           stdout=output, timeout=20, check=True)
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"invalid BIOS loader {name} in boot media")

    for name in ("kernel", "initrd"):
        os.link(work / name, tree / "boot" / name)
    (bios / "isolinux.cfg").write_text("DEFAULT psios\nPROMPT 0\nTIMEOUT 1\nLABEL psios\n"
                                       f" KERNEL /boot/kernel\n APPEND initrd=/boot/initrd {CMDLINE}\n")
    return ["-b", "boot/syslinux/isolinux.bin", "-c", "boot/syslinux/boot.cat", "-no-emul-boot",
            "-boot-load-size", "4", "-boot-info-table", "-eltorito-alt-boot"]


def build(work, signing=None):
    tree = work / "cd"
    uefi = tree / "boot/efi.img"
    uefi.parent.mkdir(parents=True)
    bios = [] if signing else bios_loader(work, tree)
    efi = secure_efi_application(work, CMDLINE, signing) if signing else efi_application(work, CMDLINE)
    with uefi.open("xb") as image:
        image.truncate((efi.stat().st_size // (4 * MIB) + 2) * 4 * MIB)
    run(work, ["mkfs.fat", "--invariant", "-n", "PSIOSRESCUE", uefi])
    run(work, ["mmd", "-i", uefi, "::/EFI", "::/EFI/BOOT"])
    run(work, ["mcopy", "-m", "-i", uefi, efi, "::/EFI/BOOT/BOOTX64.EFI"])
    image = work / "rescue.iso"
    run(work, ["xorriso", "-as", "mkisofs", "-quiet", "-R", "-V", "PSIOS_RESCUE", "--modification-date=2000010100000000",
         "--set_all_file_dates", "2000010100000000", "-uid", "0", "-gid", "0", "-file-mode", "0444",
         "-dir-mode", "0555", *bios, "-e", "boot/efi.img",
         "-no-emul-boot", "-o", image, tree])
    return image


def firmware_args(mode, work, ovmf, secure_vars=None):
    if mode == "bios":
        return ["-machine", "pc"]
    code = "OVMF_CODE_4M.secboot.fd" if secure_vars else "OVMF_CODE_4M.fd"
    shutil.copyfile(ovmf / code, work / "code.fd")
    if secure_vars:
        with regular_file(secure_vars) as source:
            variables = source.read(4 * MIB)
        if not 0 < len(variables) < 4 * MIB:
            raise ValueError("unexpected OVMF variable-store size")
        (work / "vars.fd").write_bytes(variables)
    else:
        shutil.copyfile(ovmf / "OVMF_VARS_4M.fd", work / "vars.fd")
    if (work / "code.fd").stat().st_size + (work / "vars.fd").stat().st_size != 4 * MIB:
        raise ValueError("expected a matching 4 MiB OVMF code and variable-store pair")
    machine = ["-machine", "q35,smm=on", "-global", "driver=cfi.pflash01,property=secure,value=on"] if secure_vars else ["-machine", "q35"]
    return [*machine, "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=code.fd",
            "-drive", "if=pflash,format=raw,unit=1,file=vars.fd"]


def publish(source, output):
    with tempfile.NamedTemporaryFile(dir=output.parent, prefix=".psios-media-") as stage:
        with source.open("rb") as src:
            shutil.copyfileobj(src, stage)
        stage.flush()
        os.fsync(stage.fileno())
        os.link(stage.name, output)
        parent = os.open(output.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
