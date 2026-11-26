//! Observe a whole disk and seal its Linux identity and geometry for later comparison.
mod attachment;

use crate::{Result, err, host::Work, now, require};
use psios_api::{canonical, sha256};
use rustix::{
    fd::OwnedFd,
    fs::{Dir, FileType, Mode, OFlags, ResolveFlags, fstat, fstatfs, open, openat, openat2},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::CStr,
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub const TOOL: &str = "node.block_layout";
const BLOCK_CLASS: &str = "/sys/dev/block";
const SYSFS_MAGIC: u32 = 0x6265_6572;
const ATTRIBUTE_BYTES: u64 = 4096;
const LAYOUT_ROWS: usize = 512;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);
const BELOW: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

fn decimal<T: std::str::FromStr + ToString>(value: &str) -> Option<T> {
    let number = value.parse::<T>().ok()?;
    (number.to_string() == value).then_some(number)
}

pub(super) fn device_number(value: &str) -> Option<(u32, u32)> {
    let (major, minor) = value.split_once(':')?;
    let (major, minor) = (decimal(major)?, decimal(minor)?);
    (major <= 4095 && minor <= 1048575).then_some((major, minor))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fence {
    pub(super) boot_id: String,
    pub(super) disk_sequence: String,
    pub(super) layout_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) attachment_digest: Option<String>,
}

impl Fence {
    pub(super) fn valid(&self) -> bool {
        valid_boot(&self.boot_id)
            && valid_sequence(&self.disk_sequence)
            && valid_digest(&self.layout_digest)
            && self.attachment_digest.as_deref().is_none_or(valid_digest)
    }
}

pub(super) fn valid_boot(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

pub(super) fn valid_sequence(value: &str) -> bool {
    decimal::<u64>(value).is_some_and(|sequence| sequence > 0)
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub major_minor: String,
    expected: Option<Fence>,
    #[serde(default)]
    trackable: bool,
}

impl Query {
    pub fn validate(&self) -> Result<()> {
        require(
            device_number(&self.major_minor).is_some(),
            "InvalidArgument",
            "invalid block major:minor",
        )?;
        if let Some(expected) = &self.expected {
            require(
                expected.valid() && expected.attachment_digest.is_some() == self.trackable,
                "InvalidArgument",
                "invalid expected block layout identity",
            )?;
        }
        Ok(())
    }
}

fn sysfs(dir: &OwnedFd) -> Result<()> {
    require(
        fstatfs(dir).map_err(io::Error::from)?.f_type as u32 == SYSFS_MAGIC,
        "UnsupportedCapability",
        "block layout requires Linux sysfs",
    )
}

fn lookup(device: &str) -> Result<(PathBuf, OwnedFd)> {
    let path =
        fs::canonicalize(Path::new(BLOCK_CLASS).join(device)).map_err(|error| {
            match error.kind() {
                io::ErrorKind::NotFound => err("NotFound", "block device is absent"),
                _ => error.into(),
            }
        })?;
    require(
        path.starts_with("/sys/devices"),
        "HostFailure",
        "block device is outside the kernel device tree",
    )?;
    let dir = open(&path, DIRECTORY | OFlags::NOFOLLOW, Mode::empty()).map_err(io::Error::from)?;
    sysfs(&dir)?;
    Ok((path, dir))
}

fn attribute(dir: &OwnedFd, name: &str) -> io::Result<String> {
    let file = openat2(
        dir,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        BELOW,
    )
    .map_err(io::Error::from)?;
    let mut result = String::new();
    File::from(file)
        .take(ATTRIBUTE_BYTES + 1)
        .read_to_string(&mut result)?;
    if result.len() as u64 > ATTRIBUTE_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(result.trim().to_owned())
}

fn optional(dir: &OwnedFd, name: &str) -> Result<Option<String>> {
    match attribute(dir, name) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn number(dir: &OwnedFd, field: &str) -> Result<u64> {
    decimal(&attribute(dir, field)?).ok_or_else(|| err("HostFailure", "invalid sysfs block number"))
}

fn flag(dir: &OwnedFd, field: &str) -> Result<bool> {
    match attribute(dir, field)?.as_str() {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(err("HostFailure", "invalid sysfs block flag")),
    }
}

fn device(dir: &OwnedFd) -> Result<String> {
    let value = attribute(dir, "dev")?;
    require(
        device_number(&value).is_some(),
        "HostFailure",
        "invalid sysfs device number",
    )?;
    Ok(value)
}

fn take_row(remaining: &mut usize) -> Result<()> {
    require(
        *remaining > 0,
        "QuotaExceeded",
        "block layout exceeds observation row limit",
    )?;
    *remaining -= 1;
    Ok(())
}

fn child(dir: &OwnedFd, name: &CStr) -> io::Result<OwnedFd> {
    openat2(dir, name, DIRECTORY, Mode::empty(), BELOW).map_err(io::Error::from)
}

fn edges(dir: &OwnedFd, relation: &CStr, remaining: &mut usize) -> Result<Vec<String>> {
    let relations = child(dir, relation)?;
    let mut result = Vec::new();
    for entry in Dir::read_from(&relations).map_err(io::Error::from)? {
        let entry = entry.map_err(io::Error::from)?;
        if entry.file_type() != FileType::Symlink {
            continue;
        }
        take_row(remaining)?;
        // The kernel publishes holders and slaves as links to other block devices.
        let peer = openat(&relations, entry.file_name(), DIRECTORY, Mode::empty())
            .map_err(io::Error::from)?;
        sysfs(&peer)?;
        result.push(device(&peer)?);
    }
    result.sort_unstable();
    result.dedup();
    Ok(result)
}

#[derive(PartialEq, Eq, Serialize)]
struct HardwareHint {
    source: &'static str,
    digest: String,
}

fn hardware_hint(dir: &OwnedFd) -> Option<HardwareHint> {
    let device = openat(dir, c"device", DIRECTORY, Mode::empty())
        .ok()
        .filter(|dev| sysfs(dev).is_ok());
    for (source, attr) in [("WWID", "wwid"), ("Serial", "serial")] {
        for root in std::iter::once(dir).chain(device.as_ref()) {
            if let Ok(value) = attribute(root, attr)
                && !value.is_empty()
                && value.len() <= 256
                && value.bytes().all(|byte| (32..127).contains(&byte))
            {
                return Some(HardwareHint {
                    source,
                    digest: sha256(&canonical(&json!([source, value]))),
                });
            }
        }
    }
    None
}

#[derive(PartialEq, Eq, Serialize)]
struct Partition {
    kernel_name: String,
    major_minor: String,
    number: u32,
    start_sectors_512: String,
    size_sectors_512: String,
    read_only: bool,
    holders: Vec<String>,
}

fn partitions(dir: &OwnedFd, remaining: &mut usize) -> Result<Vec<Partition>> {
    let mut parts = Vec::new();
    for entry in Dir::read_from(dir).map_err(io::Error::from)? {
        let entry = entry.map_err(io::Error::from)?;
        let name = entry.file_name();
        if entry.file_type() != FileType::Directory || matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let child = child(dir, name)?;
        let Some(partition_number) = optional(&child, "partition")? else {
            continue;
        };
        take_row(remaining)?;
        parts.push(Partition {
            kernel_name: name.to_string_lossy().into_owned(),
            major_minor: device(&child)?,
            number: decimal(&partition_number)
                .ok_or_else(|| err("HostFailure", "invalid sysfs partition number"))?,
            start_sectors_512: number(&child, "start")?.to_string(),
            size_sectors_512: number(&child, "size")?.to_string(),
            read_only: flag(&child, "ro")?,
            holders: edges(&child, c"holders", remaining)?,
        });
    }
    parts.sort_unstable_by_key(|part| part.number);
    require(
        parts
            .windows(2)
            .all(|pair| pair[0].number != pair[1].number),
        "Conflict",
        "partition numbers changed during observation",
    )?;
    Ok(parts)
}

#[derive(PartialEq, Eq, Serialize)]
struct Layout {
    sysfs_path: PathBuf,
    major_minor: String,
    disk_sequence: String,
    size_sectors_512: String,
    read_only: bool,
    removable: bool,
    logical_block_bytes: u64,
    physical_block_bytes: u64,
    hardware_hint: Option<HardwareHint>,
    holders: Vec<String>,
    backing_devices: Vec<String>,
    partitions: Vec<Partition>,
}

fn disk_sequence(dir: &OwnedFd) -> Result<String> {
    let sequence = optional(dir, "diskseq")?.ok_or_else(|| {
        err(
            "UnsupportedCapability",
            "kernel disk sequence is not exposed",
        )
    })?;
    require(
        valid_sequence(&sequence),
        "HostFailure",
        "invalid kernel disk sequence",
    )?;
    Ok(sequence)
}

fn layout(dir: &OwnedFd, path: PathBuf, expected_device: &str) -> Result<Layout> {
    let dev = device(dir)?;
    require(
        dev == expected_device,
        "Conflict",
        "block device number changed",
    )?;
    require(
        optional(dir, "partition")?.is_none(),
        "InvalidArgument",
        "a whole block device is required",
    )?;
    let mut remaining = LAYOUT_ROWS;
    Ok(Layout {
        sysfs_path: path,
        major_minor: dev,
        disk_sequence: disk_sequence(dir)?,
        size_sectors_512: number(dir, "size")?.to_string(),
        read_only: flag(dir, "ro")?,
        removable: flag(dir, "removable")?,
        logical_block_bytes: number(dir, "queue/logical_block_size")?,
        physical_block_bytes: number(dir, "queue/physical_block_size")?,
        hardware_hint: hardware_hint(dir),
        holders: edges(dir, c"holders", &mut remaining)?,
        backing_devices: edges(dir, c"slaves", &mut remaining)?,
        partitions: partitions(dir, &mut remaining)?,
    })
}

fn same_directory(first: &OwnedFd, second: &OwnedFd) -> Result<bool> {
    let (old, new) = (
        fstat(first).map_err(io::Error::from)?,
        fstat(second).map_err(io::Error::from)?,
    );
    Ok(old.st_dev == new.st_dev && old.st_ino == new.st_ino)
}

pub(super) struct Sample {
    pub(super) fence: Fence,
    pub(super) layout: Value,
    pub(super) mount_namespace_inode: String,
    pub(super) observed_at: String,
}

pub(super) enum Presence {
    Absent,
    SameSequence,
    OtherSequence,
}

pub(super) fn presence(major_minor: &str, expected_sequence: &str) -> Result<Presence> {
    let (path, first) = match lookup(major_minor) {
        Ok(found) => found,
        Err(error) if error.0.code == "NotFound" => {
            return match lookup(major_minor) {
                Err(error) if error.0.code == "NotFound" => Ok(Presence::Absent),
                Err(error) => Err(error),
                Ok(_) => Err(err("Conflict", "block device appeared during observation")),
            };
        }
        Err(error) => return Err(error),
    };
    let sequence = disk_sequence(&first)?;
    let (current_path, current) = lookup(major_minor)?;
    require(
        path == current_path
            && same_directory(&first, &current)?
            && device(&first)? == major_minor
            && device(&current)? == major_minor
            && sequence == disk_sequence(&current)?,
        "Conflict",
        "block device changed during identity observation",
    )?;
    Ok(if sequence == expected_sequence {
        Presence::SameSequence
    } else {
        Presence::OtherSequence
    })
}

pub(super) fn sample(major_minor: &str, trackable: bool) -> Result<Sample> {
    let boot_id = crate::linux_clock::BootClock::sample()?.boot_id;
    let namespace = fs::metadata("/proc/self/ns/mnt")?.ino();
    let (path, directory) = lookup(major_minor)?;
    let first = layout(&directory, path, major_minor)?;
    let attachment = trackable
        .then(|| attachment::Ownership::capture(&first))
        .transpose()?;
    let (path, reopened) = lookup(major_minor)?;
    require(
        same_directory(&directory, &reopened)? && first == layout(&reopened, path, major_minor)?,
        "Conflict",
        "block layout changed during observation",
    )?;
    let attachment_digest = attachment.map(|proof| proof.confirm(&first)).transpose()?;
    let value = serde_json::to_value(&first)?;
    let fence = Fence {
        boot_id,
        disk_sequence: first.disk_sequence,
        layout_digest: sha256(&canonical(&value)),
        attachment_digest,
    };
    Ok(Sample {
        fence,
        layout: value,
        mount_namespace_inode: namespace.to_string(),
        observed_at: now(),
    })
}

pub(super) fn observe(w: &Work, query: &Query) -> Result<Value> {
    let sample = sample(&query.major_minor, query.trackable)?;
    require(
        query
            .expected
            .as_ref()
            .is_none_or(|expected| expected == &sample.fence),
        "PreconditionFailed",
        "block layout identity or geometry has changed",
    )?;
    Ok(
        json!({"node_ref":w.target.reference(),"source":"LinuxSysfs","fence":sample.fence,"layout":sample.layout,
               "mount_namespace_inode":sample.mount_namespace_inode,"observed_at":sample.observed_at}),
    )
}
