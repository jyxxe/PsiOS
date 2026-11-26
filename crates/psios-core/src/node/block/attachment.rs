//! Identity evidence for the direct or read-only loop profiles.
use super::{Layout, decimal, device_number};
use crate::{Result, err, require};
use psios_api::{canonical, sha256};
use rustix::{
    fd::OwnedFd,
    fs::{FileType, Mode, OFlags, fstat, major, minor, open},
    ioctl::{Getter, ioctl, opcode},
};
use serde_json::json;
use std::{io, path::Path};

const BLOCK_DISK_SEQUENCE: rustix::ioctl::Opcode = opcode::read::<u64>(0x12, 128);
const LOOP_STATUS: rustix::ioctl::Opcode = 0x4C05;
const LOOP_READ_ONLY: u32 = 1;

// Linux UAPI loop_info64 from linux/loop.h. The names and key bytes are
// never read by the authority or included in the proof.
#[repr(C)]
struct LoopInfo {
    device: u64,
    inode: u64,
    backing_rdev: u64,
    offset: u64,
    size_limit: u64,
    number: u32,
    encryption: u32,
    encryption_size: u32,
    flags: u32,
    file_name: [u8; 64],
    crypt_name: [u8; 64],
    key: [u8; 32],
    reserved: [u64; 2],
}

const _: () = assert!(size_of::<LoopInfo>() == 232);

pub(super) enum Ownership {
    Direct,
    Loop {
        fd: OwnedFd,
        number: u32,
        digest: String,
    },
}

fn loop_open(layout: &Layout) -> Result<(OwnedFd, u32)> {
    let name = layout
        .sysfs_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| err("UnsupportedCapability", "no verified loop device name"))?;
    let number = name
        .strip_prefix("loop")
        .and_then(decimal::<u32>)
        .ok_or_else(|| {
            err(
                "UnsupportedCapability",
                "only direct disks and read-only loops can be tracked",
            )
        })?;
    let dev = device_number(&layout.major_minor)
        .filter(|(major, _)| *major == 7 && layout.read_only)
        .ok_or_else(|| {
            err(
                "UnsupportedCapability",
                "loop identity requires a read-only whole device",
            )
        })?;
    let fd = open(
        Path::new("/dev").join(name),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| {
        err(
            "DependencyUnavailable",
            "unable to open the kernel loop device for identity",
        )
    })?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    require(
        FileType::from_raw_mode(stat.st_mode).is_block_device()
            && (major(stat.st_rdev), minor(stat.st_rdev)) == dev,
        "Conflict",
        "loop descriptor differs from the kernel device number",
    )?;
    Ok((fd, number))
}

fn loop_digest(fd: &OwnedFd, number: u32, disk_sequence: &str) -> Result<String> {
    let sequence = || {
        // SAFETY: BLKGETDISKSEQ reads exactly one u64 into the kernel output buffer.
        unsafe { ioctl(fd, Getter::<BLOCK_DISK_SEQUENCE, u64>::new()) }.map_err(|_| {
            err(
                "DependencyUnavailable",
                "cannot verify sequence on the loop descriptor",
            )
        })
    };
    let before = sequence()?;
    // SAFETY: this Linux loop ioctl writes loop_info64, with the checked C layout above.
    let info = unsafe { ioctl(fd, Getter::<LOOP_STATUS, LoopInfo>::new()) }.map_err(|_| {
        err(
            "DependencyUnavailable",
            "cannot verify the attached loop backing file",
        )
    })?;
    require(
        Some(before) == decimal::<u64>(disk_sequence)
            && sequence()? == before
            && info.number == number,
        "Conflict",
        "loop descriptor identity changed during observation",
    )?;
    require(
        info.flags & LOOP_READ_ONLY != 0
            && info.device != 0
            && info.inode != 0
            && info.backing_rdev == 0
            && info.encryption == 0
            && info.encryption_size == 0,
        "UnsupportedCapability",
        "loop identity requires an unencrypted read-only regular-file attachment",
    )?;
    Ok(sha256(&canonical(&json!([
        "psios/linux-block-attachment/1",
        "ReadOnlyLoop",
        number,
        info.device.to_string(),
        info.inode.to_string(),
        info.offset.to_string(),
        info.size_limit.to_string()
    ]))))
}

impl Ownership {
    pub(super) fn capture(layout: &Layout) -> Result<Self> {
        require(
            !layout.removable
                && layout.size_sectors_512 != "0"
                && layout.backing_devices.is_empty(),
            "UnsupportedCapability",
            "trackable block devices require non-removable unstacked media",
        )?;
        if layout.sysfs_path.starts_with("/sys/devices/virtual/block") {
            let (fd, number) = loop_open(layout)?;
            let digest = loop_digest(&fd, number, &layout.disk_sequence)?;
            Ok(Self::Loop { fd, number, digest })
        } else {
            require(
                !layout.sysfs_path.starts_with("/sys/devices/virtual"),
                "UnsupportedCapability",
                "the virtual block transport cannot prove attachment identity",
            )?;
            Ok(Self::Direct)
        }
    }

    pub(super) fn confirm(self, layout: &Layout) -> Result<String> {
        match self {
            Self::Direct => Ok(sha256(&canonical(&json!([
                "psios/linux-block-attachment/1",
                "DirectSysfs",
                layout.sysfs_path
            ])))),
            Self::Loop { fd, number, digest } => {
                require(
                    loop_digest(&fd, number, &layout.disk_sequence)? == digest,
                    "Conflict",
                    "loop backing identity changed during observation",
                )?;
                Ok(digest)
            }
        }
    }
}
