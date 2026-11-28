"""Linux block-device custody for a native blank-disk installation."""

from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import re
import stat
import struct
import uuid


SYSFS = Path("/sys")
DEVICES = SYSFS / "devices"
BLOCK = SYSFS / "dev/block"
VIRTIO = SYSFS / "bus/virtio"
VIRTIO_DRIVER = VIRTIO / "drivers/virtio_blk"
VIRTIO_NAME = re.compile(r"vd[a-z]{1,5}\Z")
PCI = SYSFS / "bus/pci"
NVME_DRIVER = PCI / "drivers/nvme"
NVME_NAME = re.compile(r"nvme[0-9]{1,7}n[0-9]{1,10}\Z")
DEVICE_SERIAL = re.compile(r"[ -~]{1,20}\Z")
NVME_WWID = re.compile(r"(?:uuid|eui|nvme)\.[!-~]{1,240}\Z")
NVME_EUI = re.compile(r"eui\.([0-9a-f]{16}|[0-9a-f]{32})\Z")
NVME_UUID = re.compile(r"uuid\.([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})\Z")
AHCI_DRIVER = PCI / "drivers/ahci"
SCSI = SYSFS / "bus/scsi"
SCSI_DISK_DRIVER = SCSI / "drivers/sd"
SCSI_DISK_NAME = re.compile(r"sd[a-z]{1,6}\Z")
ATA_WWID = re.compile(r"naa\.5[0-9a-f]{15}\Z")
LOOP_NAME = re.compile(r"loop(?:0|[1-9][0-9]{0,6})\Z")
SERIAL = re.compile(r"[A-Za-z0-9_.-]{1,20}\Z")
UINT = re.compile(r"0|[1-9][0-9]{0,19}\Z")
MAX_SYSFS_ENTRIES = 1024

# Linux amd64 block and loop UAPI. The size fields use native byte order.
BLKROGET = 0x125E
BLKSSZGET = 0x1268
BLKPBSZGET = 0x127B
BLKGETSIZE64 = 0x80081272
BLKGETDISKSEQ = 0x80081280
LOOP_GET_STATUS64 = 0x4C05
LOOP_STATUS64 = struct.Struct("=5Q4I64x64x32x16x")
U32 = struct.Struct("=I")
U64 = struct.Struct("=Q")
LOOP_PARTSCAN = 8
LOOP_DIRECT_IO = 16

SAFE_OPEN = os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK


def _kind(path):
    if path.parent != Path("/dev"):
        raise ValueError("installation target must be a canonical /dev whole-device path")
    if VIRTIO_NAME.fullmatch(path.name):
        return "VirtioBlock"
    if NVME_NAME.fullmatch(path.name):
        return "NVMeNamespace"
    if SCSI_DISK_NAME.fullmatch(path.name):
        return "SataAhciDisk"
    if LOOP_NAME.fullmatch(path.name):
        return "TestLoop"
    raise ValueError("installation target is outside the supported whole-device profile")


def _device_stat(fd, path):
    label = path.stat(follow_symlinks=False)
    opened = os.fstat(fd)
    if (not stat.S_ISBLK(opened.st_mode) or not stat.S_ISBLK(label.st_mode)
            or (opened.st_dev, opened.st_ino, opened.st_rdev) != (label.st_dev, label.st_ino, label.st_rdev)):
        raise ValueError("installation target path and held block device do not agree")
    return opened


@contextmanager
def open_target(path: Path, *, write: bool):
    """Hold one exclusive Linux block FD until installation or inspection completes."""
    _kind(path)
    if write and os.geteuid() != 0:
        raise ValueError("opening an installation target for writing requires local root")
    fd = os.open(path, SAFE_OPEN | os.O_EXCL | (os.O_RDWR if write else os.O_RDONLY))
    try:
        _device_stat(fd, path)
        yield fd
    finally:
        os.close(fd)


@contextmanager
def _directory(path, *, dir_fd=None):
    fd = os.open(path, SAFE_OPEN | os.O_RDONLY | os.O_DIRECTORY, dir_fd=dir_fd)
    try:
        yield fd
    finally:
        os.close(fd)


def _ioctl(fd, code, layout):
    result = bytearray(layout.size)
    fcntl.ioctl(fd, code, result, True)
    return layout.unpack(result)


def _read_attribute_bytes(fd, name):
    attribute = os.open(name, SAFE_OPEN | os.O_RDONLY, dir_fd=fd)
    with os.fdopen(attribute, "rb") as file:
        info = os.fstat(file.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_dev != os.fstat(fd).st_dev:
            raise ValueError("invalid Linux sysfs attribute")
        value = file.read(4097)
    if len(value) > 4096:
        raise ValueError("Linux sysfs attribute exceeds the reader limit")
    return value


def _read_attribute(fd, name):
    return _read_attribute_bytes(fd, name).decode("ascii").removesuffix("\n")


def _number(fd, name):
    value = _read_attribute(fd, name)
    if not UINT.fullmatch(value):
        raise ValueError(f"invalid Linux block attribute: {name}")
    return int(value)


def filesystem_type(fd):
    with Path(f"/proc/self/fdinfo/{fd}").open("rb") as info:
        mount = next((line.split()[1] for line in info if line.startswith(b"mnt_id:\t")), None)
    if mount is not None:
        with Path("/proc/self/mountinfo").open("rb") as mounts:
            for index, line in enumerate(mounts):
                if index >= 8192:
                    break
                if line.split(b" ", 1)[0] == mount:
                    _, _, details = line.partition(b" - ")
                    return details.split(b" ", 1)[0]
    return b""


def _sysfs_device():
    with _directory(SYSFS) as fd:
        if filesystem_type(fd) == b"sysfs":
            return os.fstat(fd).st_dev
    raise ValueError("installation target requires the Linux sysfs device tree")


def _kernel_path(path, name):
    resolved = path.resolve(strict=True)
    if not resolved.is_relative_to(DEVICES) or resolved.name != name:
        raise ValueError("installation target is outside the expected kernel device tree")
    return resolved


def _whole_disk(directory):
    try:
        os.stat("partition", dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return True
    return False


@contextmanager
def _disk(fd, path):
    opened = _device_stat(fd, path)
    device = f"{os.major(opened.st_rdev)}:{os.minor(opened.st_rdev)}"
    device_link = BLOCK / device
    kernel_path = _kernel_path(device_link, path.name)
    sysfs_device = _sysfs_device()
    sequence, = _ioctl(fd, BLKGETDISKSEQ, U64)
    with _directory(kernel_path) as directory:
        if (os.fstat(directory).st_dev != sysfs_device or not _whole_disk(directory)
                or _read_attribute(directory, "dev") != device or sequence == 0
                or _number(directory, "diskseq") != sequence):
            raise ValueError("installation target does not match a whole disk generation")
        yield directory, kernel_path, device, sequence, sysfs_device
        if (_device_stat(fd, path).st_rdev != opened.st_rdev
                or _kernel_path(device_link, path.name) != kernel_path
                or _ioctl(fd, BLKGETDISKSEQ, U64) != (sequence,)
                or _number(directory, "diskseq") != sequence):
            raise ValueError("installation target generation changed during inspection")


def _virtio_device(kernel_path):
    device = kernel_path.parent.parent
    if (kernel_path.parent.name != "block" or not re.fullmatch(r"virtio[0-9]+", device.name)
            or (device / "subsystem").resolve(strict=True) != VIRTIO
            or (device / "driver").resolve(strict=True) != VIRTIO_DRIVER):
        raise ValueError("installation target is not a whole disk from the Linux virtio_blk driver")
    return device


def _block_names(directory, pattern):
    names = set()
    with os.scandir(directory) as entries:
        for index, entry in enumerate(entries):
            if index >= MAX_SYSFS_ENTRIES:
                raise ValueError("Linux block inventory exceeds the installation target limit")
            if pattern.fullmatch(entry.name):
                names.add(entry.name)
    return names


def _virtio_identity(directory, kernel_path, sysfs_device):
    device = _virtio_device(kernel_path)
    serial = _read_attribute(directory, "serial")
    if not SERIAL.fullmatch(serial):
        raise ValueError("virtio installation target requires a stable printable serial")
    with _directory(SYSFS / "block") as block:
        if os.fstat(block).st_dev != sysfs_device:
            raise ValueError("Linux block index is outside sysfs")
        names = _block_names(block, VIRTIO_NAME)
        if kernel_path.name not in names:
            raise ValueError("installation target is absent from the Linux block index")
        for name in sorted(names - {kernel_path.name}):
            sibling = _kernel_path(SYSFS / "block" / name, name)
            _virtio_device(sibling)
            with _directory(sibling) as other:
                if os.fstat(other).st_dev != sysfs_device:
                    raise ValueError("Linux virtio inventory is outside sysfs")
                if _read_attribute(other, "serial") == serial:
                    raise ValueError("virtio installation serial is not unique")
        if _block_names(block, VIRTIO_NAME) != names or _read_attribute(directory, "serial") != serial:
            raise ValueError("Linux virtio inventory changed during serial verification")
    return {"kind": "VirtioBlock", "serial": serial, "sysfs_path": str(device)}


def _nvme_attributes(directory, kernel_path, sysfs_device):
    controller = kernel_path.parent
    pci = controller.parent.parent
    if (controller.parent.name != "nvme" or not re.fullmatch(r"nvme[0-9]+", controller.name)
            or (pci / "subsystem").resolve(strict=True) != PCI
            or (pci / "driver").resolve(strict=True) != NVME_DRIVER):
        raise ValueError("installation target is not a local PCIe NVMe namespace")
    with _directory(controller) as opened:
        if os.fstat(opened).st_dev != sysfs_device or _read_attribute(opened, "transport") != "pcie":
            raise ValueError("Linux NVMe controller is outside the PCIe profile")
        serial = _read_attribute(opened, "serial").strip(" ")
    return controller, serial, _number(directory, "nsid"), _read_attribute(directory, "wwid")


def _nvme_identity(directory, kernel_path, sysfs_device):
    observed = _nvme_attributes(directory, kernel_path, sysfs_device)
    controller, serial, namespace_id, wwid = observed
    if not DEVICE_SERIAL.fullmatch(serial):
        raise ValueError("NVMe installation target requires a stable printable serial")
    if not 0 < namespace_id < 0xffffffff or not NVME_WWID.fullmatch(wwid):
        raise ValueError("NVMe installation target requires a valid namespace ID and WWID")
    with _directory(SYSFS / "block") as block:
        if os.fstat(block).st_dev != sysfs_device:
            raise ValueError("Linux block index is outside sysfs")
        names = _block_names(block, NVME_NAME)
        if kernel_path.name not in names:
            raise ValueError("installation target is absent from the Linux block index")
        for name in sorted(names - {kernel_path.name}):
            sibling = _kernel_path(SYSFS / "block" / name, name)
            with _directory(sibling) as other:
                if os.fstat(other).st_dev != sysfs_device:
                    raise ValueError("Linux NVMe inventory is outside sysfs")
                _, other_serial, other_namespace, other_wwid = _nvme_attributes(other, sibling, sysfs_device)
                if (other_serial, other_namespace) == (serial, namespace_id) or other_wwid == wwid:
                    raise ValueError("NVMe installation namespace identity is not unique")
        if (_block_names(block, NVME_NAME) != names
                or _nvme_attributes(directory, kernel_path, sysfs_device) != observed):
            raise ValueError("Linux NVMe inventory changed during namespace verification")
    return {"kind": "NVMeNamespace", "serial": serial, "namespace_id": namespace_id,
            "wwid": wwid, "sysfs_path": str(controller)}


def _ahci_device(kernel_path):
    disk = kernel_path.parent.parent
    host = disk.parent.parent
    ata = host.parent
    pci = ata.parent
    if (kernel_path.parent.name != "block" or not re.fullmatch(r"ata[0-9]+", ata.name)
            or not re.fullmatch(r"host[0-9]+", host.name)
            or (disk / "subsystem").resolve(strict=True) != SCSI
            or (disk / "driver").resolve(strict=True) != SCSI_DISK_DRIVER
            or (pci / "subsystem").resolve(strict=True) != PCI
            or (pci / "driver").resolve(strict=True) != AHCI_DRIVER):
        return None
    return disk, pci


def _ata_serial(device):
    try:
        page = _read_attribute_bytes(device, "vpd_pg80")
    except FileNotFoundError:
        return None
    # The SCSI translation of ATA IDENTIFY exposes the 20-byte serial in VPD page 80.
    if len(page) < 4 or page[1] != 0x80 or int.from_bytes(page[2:4], "big") != len(page) - 4:
        return None
    try:
        serial = page[4:].decode("ascii").strip(" ")
    except UnicodeDecodeError:
        return None
    return serial if DEVICE_SERIAL.fullmatch(serial) else None


def _ahci_attributes(device, sysfs_device):
    with _directory(device) as opened:
        if os.fstat(opened).st_dev != sysfs_device or _number(opened, "type") != 0:
            raise ValueError("SATA installation target requires a local direct-access disk")
        return _ata_serial(opened), _read_attribute(opened, "wwid")


def _ahci_identity(directory, kernel_path, sysfs_device):
    location = _ahci_device(kernel_path)
    if location is None or _number(directory, "removable") != 0:
        raise ValueError("installation target is not a fixed SATA disk from the PCI AHCI driver")
    device, controller = location
    with _directory(controller) as pci:
        if os.fstat(pci).st_dev != sysfs_device or _read_attribute(pci, "class") != "0x010601":
            raise ValueError("installation target is outside the PCI SATA AHCI profile")
    serial, wwid = _ahci_attributes(device, sysfs_device)
    if serial is None:
        raise ValueError("SATA installation target requires a stable printable serial")
    if not ATA_WWID.fullmatch(wwid):
        raise ValueError("SATA installation target requires a registered NAA WWID")
    with _directory(SYSFS / "block") as block:
        if os.fstat(block).st_dev != sysfs_device:
            raise ValueError("Linux block index is outside sysfs")
        names = _block_names(block, SCSI_DISK_NAME)
        if kernel_path.name not in names:
            raise ValueError("installation target is absent from the Linux block index")
        for name in sorted(names - {kernel_path.name}):
            sibling = _kernel_path(SYSFS / "block" / name, name)
            other = _ahci_device(sibling)
            if other is None:
                continue
            serial_other, wwid_other = _ahci_attributes(other[0], sysfs_device)
            if serial_other == serial or wwid_other == wwid:
                raise ValueError("SATA installation disk identity is not unique")
        if (_block_names(block, SCSI_DISK_NAME) != names or _ahci_device(kernel_path) != location
                or _ahci_attributes(device, sysfs_device) != (serial, wwid)):
            raise ValueError("Linux SATA inventory changed during identity verification")
    return {"kind": "SataAhciDisk", "serial": serial, "wwid": wwid, "sysfs_path": str(device)}


def _loop_identity(fd, path, kernel_path):
    if kernel_path != DEVICES / "virtual/block" / path.name:
        raise ValueError("test loop target is outside the Linux virtual block tree")
    backing, inode, rdev, offset, limit, number, cipher, key_size, flags = _ioctl(
        fd, LOOP_GET_STATUS64, LOOP_STATUS64)
    if (not backing or not inode or rdev or offset or limit or cipher or key_size
            or number != int(path.name[4:]) or flags & ~(LOOP_PARTSCAN | LOOP_DIRECT_IO)):
        raise ValueError("test loop target requires a writable whole regular-file backing")
    return {"kind": "TestLoop", "backing_device": backing, "backing_inode": inode,
            "offset_bytes": offset, "size_limit_bytes": limit, "flags": flags}


def _writable_geometry(fd, directory, sysfs_device):
    size, = _ioctl(fd, BLKGETSIZE64, U64)
    logical, = _ioctl(fd, BLKSSZGET, U32)
    physical, = _ioctl(fd, BLKPBSZGET, U32)
    readonly, = _ioctl(fd, BLKROGET, U32)
    with _directory("queue", dir_fd=directory) as queue:
        if (os.fstat(queue).st_dev != sysfs_device or size == 0 or size % 512 or logical != 512
                or physical not in (512, 1024, 2048, 4096)
                or _number(directory, "size") * 512 != size
                or _number(queue, "logical_block_size") != logical
                or _number(queue, "physical_block_size") != physical):
            raise ValueError("installation target geometry is unsupported or changed")
    if readonly != 0 or _number(directory, "ro") != 0:
        raise ValueError("installation target is read-only")
    return size, logical, physical


def kernel_boot_id():
    boot_id = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").removesuffix("\n")
    if str(uuid.UUID(boot_id)) != boot_id:
        raise ValueError("invalid Linux boot identity")
    return boot_id


def read_target(fd, path: Path, *, allow_loop: bool) -> dict:
    """Read and cross-check the identity of the block FD held by open_target."""
    kind = _kind(path)
    if kind == "TestLoop" and not allow_loop:
        raise ValueError("loop targets require explicit test-only opt-in")
    boot_id = kernel_boot_id()
    with _disk(fd, path) as (directory, kernel_path, device, sequence, sysfs_device):
        size, logical, physical = _writable_geometry(fd, directory, sysfs_device)
        if kind == "VirtioBlock":
            transport = _virtio_identity(directory, kernel_path, sysfs_device)
        elif kind == "NVMeNamespace":
            transport = _nvme_identity(directory, kernel_path, sysfs_device)
        elif kind == "SataAhciDisk":
            transport = _ahci_identity(directory, kernel_path, sysfs_device)
        else:
            transport = _loop_identity(fd, path, kernel_path)
        if ((size, logical, physical) != _writable_geometry(fd, directory, sysfs_device)
                or (kind == "TestLoop" and transport != _loop_identity(fd, path, kernel_path))):
            raise ValueError("installation target identity changed during inspection")
        return {"path": str(path), "major_minor": device, "sysfs_path": str(kernel_path),
                "boot_id": boot_id, "disk_sequence": sequence, "size_bytes": size,
                "logical_block_bytes": logical, "physical_block_bytes": physical,
                "read_only": False, "transport": transport}


def _reboot_disk_identity(target):
    transport = target.get("transport")
    boot, path = target.get("boot_id"), target.get("path")
    if (not isinstance(transport, dict) or not isinstance(path, str)
            or not isinstance(boot, str) or str(uuid.UUID(boot)) != boot):
        raise ValueError("invalid cross-boot installation target")
    kind = _kind(Path(path))
    serial, wwid = transport.get("serial"), transport.get("wwid")
    if (kind != transport.get("kind") or not isinstance(serial, str) or not DEVICE_SERIAL.fullmatch(serial)
            or not isinstance(wwid, str)):
        raise ValueError("invalid cross-boot installation disk identity")
    if kind == "SataAhciDisk" and ATA_WWID.fullmatch(wwid):
        return kind, serial, wwid
    if kind == "NVMeNamespace":
        eui = NVME_EUI.fullmatch(wwid)
        identifier = NVME_UUID.fullmatch(wwid)
        namespace = transport.get("namespace_id")
        if (type(namespace) is int and 0 < namespace < 0xffffffff
                and ((eui and int(eui[1], 16)) or (identifier and uuid.UUID(identifier[1]).int))):
            return kind, serial, namespace, wwid
        raise ValueError("cross-boot NVMe reconciliation requires a registered namespace EUI, NGUID, or UUID")
    raise ValueError("cross-boot installation reconciliation requires a fixed SATA disk or an identified NVMe namespace")


def require_reboot_target(planned, observed):
    """Require a persistent device identity in place of the expired Linux generation."""
    planned_identity = _reboot_disk_identity(planned)
    observed_identity = _reboot_disk_identity(observed)
    if planned["boot_id"] == observed["boot_id"]:
        raise ValueError("cross-boot installation reconciliation requires a different Linux boot")
    if (planned_identity != observed_identity
            or any(planned.get(key) != observed.get(key) for key in (
                "size_bytes", "logical_block_bytes", "physical_block_bytes", "read_only"))):
        raise ValueError("target disk persistent identity or geometry differs from the installation plan")


def require_unallocated(fd, path: Path):
    """Reject partition children and device relationships before a fresh write."""
    _kind(path)
    with _disk(fd, path) as (directory, _, _, _, sysfs_device):
        for relation in ("holders", "slaves"):
            with _directory(relation, dir_fd=directory) as related:
                with os.scandir(related) as entries:
                    if os.fstat(related).st_dev != sysfs_device or next(entries, None) is not None:
                        raise ValueError("installation target has active block device relationships")
        with os.scandir(directory) as entries:
            for index, entry in enumerate(entries):
                if index >= MAX_SYSFS_ENTRIES:
                    raise ValueError("Linux block device layout exceeds the installation target limit")
                if entry.is_dir(follow_symlinks=False):
                    with _directory(entry.name, dir_fd=directory) as child:
                        if os.fstat(child).st_dev != sysfs_device or not _whole_disk(child):
                            raise ValueError("installation target contains an existing partition")
