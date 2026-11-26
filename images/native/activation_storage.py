"""Verify the approved native disk's cold storage state without mounting it."""

from dataclasses import dataclass
import fcntl
import hashlib
import os
import struct
import subprocess
import uuid

import create
from install_source import RECEIPT_LIMIT, gpt_facts


BUILD_RECEIPT = "candidate-build.json"
SECTOR_BYTES = 512
SUPERBLOCK_BYTES = 1024


@dataclass(frozen=True)
class BootStorage:
    layout: create.Layout
    state_uuid: str
    home_uuid: str

    @classmethod
    def from_receipt(cls, path, approved_sha256, disk_bytes):
        with create.boot_media.regular_file(path) as file:
            raw = file.read(RECEIPT_LIMIT + 1)
        if len(raw) > RECEIPT_LIMIT or hashlib.sha256(raw).hexdigest() != approved_sha256:
            raise ValueError("candidate build receipt changed before managed boot")
        build, layout, _ = create.parse_receipt(raw)
        state, home = build["state_uuid"], build["home_uuid"]
        if layout.image_mib * create.MIB != disk_bytes or not all(
                uuid.UUID(identity).int for identity in (state, home)):
            raise ValueError("candidate storage geometry or filesystem UUID is invalid")
        return cls(layout, state, home)

    def gpt(self, work, disk_fd):
        disk = os.fstat(disk_fd)
        if disk.st_size != self.layout.image_mib * create.MIB:
            raise ValueError("managed candidate disk no longer has its approved size")
        image = os.open(f"/proc/self/fd/{disk_fd}", os.O_RDONLY | os.O_CLOEXEC)
        try:
            if not os.path.samestat(disk, os.fstat(image)):
                raise ValueError("managed candidate disk identity changed before GPT validation")
            return gpt_facts(work, f"/proc/self/fd/{image}", self.layout, pass_fds=(image,))
        except (KeyError, TypeError, ValueError, subprocess.SubprocessError) as error:
            raise ValueError("managed candidate GPT does not match the approved native layout") from error
        finally:
            os.close(image)

    def verify_shutdown(self, work, disk_fd, before=None):
        after = self.gpt(work, disk_fd)
        if before is not None and after != before:
            raise ValueError("managed candidate GPT changed during boot")
        expected = (self.state_uuid, self.home_uuid)
        for name, identity, part in zip(("state", "home"), expected, after["partitions"][1:], strict=True):
            start = part["start_sector"] * SECTOR_BYTES
            size = part["sector_count"] * SECTOR_BYTES
            if (size < 2 * SUPERBLOCK_BYTES
                    or start + size > self.layout.image_mib * create.MIB):
                raise ValueError("managed candidate ext4 superblock is outside its checked partition")
            superblock = os.pread(disk_fd, SUPERBLOCK_BYTES, start + SUPERBLOCK_BYTES)
            try:
                state = create.filesystem_superblock(superblock, identity)
            except ValueError as error:
                raise ValueError(f"managed candidate {name} ext4 identity could not be verified") from error
            if state != (1, False) or struct.unpack_from("<I", superblock, 0xE8)[0]:
                raise ValueError(f"managed candidate {name} ext4 was not cleanly shut down")
            ro_compat = struct.unpack_from("<I", superblock, 0x64)[0]
            if ro_compat & 0x400 and (superblock[0x175] != 1 or not valid_checksum(superblock)):
                raise ValueError(f"managed candidate {name} ext4 superblock checksum is invalid")


def valid_checksum(superblock):
    # ext4 uses reflected CRC32C seeded with ~0 over bytes before s_checksum.
    crc = 0xFFFFFFFF
    for byte in superblock[:0x3FC]:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc == struct.unpack_from("<I", superblock, 0x3FC)[0]


def lock_cold(fd):
    # Match the existing native source fence. Held until the controller closes
    # its exact descriptor, this also fences cooperating QEMU openers.
    lock = struct.pack("@hhqqi4x", fcntl.F_WRLCK, os.SEEK_SET, 0, 0, 0)
    try:
        fcntl.fcntl(fd, fcntl.F_OFD_SETLK, lock)
    except OSError as error:
        raise ValueError("managed candidate artifacts could not be locked after shutdown") from error
