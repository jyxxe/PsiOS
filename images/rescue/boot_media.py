"""Build a read-only El Torito rescue CD for SeaBIOS and amd64 UEFI."""
import hashlib
import os
import shutil
import subprocess
import tempfile


EPOCH = 946684800  # 2000-01-01 UTC, also representable by FAT and ISO 9660.
CMDLINE = "console=ttyS0,115200 quiet panic=1 rdinit=/psios-rescue"
MIB = 1024 * 1024


def build(work):
    tree = work / "cd"
    bios = tree / "boot/syslinux"
    bios.mkdir(parents=True)
    uefi = tree / "boot/efi.img"
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
    # Keep the UEFI loader, kernel and initrd together. GRUB boots from its own
    # memory disk without searching the subject volume for configuration or code.
    grub = work / "grub.cfg"
    grub.write_text(f"linux (memdisk)/boot/kernel {CMDLINE}\ninitrd (memdisk)/boot/initrd\nboot\n")
    for source in (grub, work / "kernel", work / "initrd"):
        os.utime(source, (EPOCH, EPOCH))
    env = {**os.environ, "LC_ALL": "C", "TZ": "UTC", "SOURCE_DATE_EPOCH": str(EPOCH)}

    def run(args):
        subprocess.run(args, cwd=work, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                       timeout=90, check=True)

    efi = work / "BOOTX64.EFI"
    modules = "linux normal memdisk tar"
    run(["grub-mkstandalone", "--format=x86_64-efi", "--modules=" + modules, "--install-modules=" + modules,
         "--locales=", "--fonts=", "--themes=", "--output", efi, "boot/grub/grub.cfg=grub.cfg",
         "boot/kernel=kernel", "boot/initrd=initrd"])
    os.utime(efi, (EPOCH, EPOCH))
    with uefi.open("xb") as image:
        image.truncate((efi.stat().st_size // (4 * MIB) + 2) * 4 * MIB)
    run(["mkfs.fat", "--invariant", "-n", "PSIOSRESCUE", uefi])
    run(["mmd", "-i", uefi, "::/EFI", "::/EFI/BOOT"])
    run(["mcopy", "-m", "-i", uefi, efi, "::/EFI/BOOT/BOOTX64.EFI"])
    image = work / "rescue.iso"
    run(["xorriso", "-as", "mkisofs", "-quiet", "-R", "-V", "PSIOS_RESCUE", "--modification-date=2000010100000000",
         "--set_all_file_dates", "2000010100000000", "-uid", "0", "-gid", "0", "-file-mode", "0444",
         "-dir-mode", "0555", "-b", "boot/syslinux/isolinux.bin", "-c", "boot/syslinux/boot.cat",
         "-no-emul-boot", "-boot-load-size", "4", "-boot-info-table", "-eltorito-alt-boot", "-e", "boot/efi.img",
         "-no-emul-boot", "-o", image, tree])
    return image


def firmware_args(mode, work, ovmf):
    if mode == "bios":
        return ["-machine", "pc"]
    for source, name in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(ovmf / source, work / name)
    if (work / "code.fd").stat().st_size + (work / "vars.fd").stat().st_size != 4 * MIB:
        raise ValueError("expected a matching 4 MiB OVMF code and variable-store pair")
    return ["-machine", "q35", "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=code.fd",
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
