use crate::{Result, err, require};
use psios_api::{DeviceInventoryCursor, DeviceInventoryQuery, MAX_DEVICE_INVENTORY_PAGE};
use rustix::{
    fd::OwnedFd,
    fs::{Mode, OFlags, ResolveFlags, fstatfs, openat2},
};
use serde::Serialize;
use std::{
    fs::File,
    io::{self, Read},
};

const SYSFS_MAGIC: u32 = 0x6265_6572;
const ATTRIBUTE_BYTES: u64 = 4096;
const INVENTORY_PAGE_BYTES: usize = 12 * 1024;

pub(super) const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);
pub(super) const BELOW: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

pub(super) fn is_sysfs(dir: &OwnedFd) -> io::Result<bool> {
    Ok(fstatfs(dir).map_err(io::Error::from)?.f_type as u32 == SYSFS_MAGIC)
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(super) fn attribute(dir: &OwnedFd, name: &str) -> io::Result<String> {
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

pub(super) fn validate_inventory(
    query: &DeviceInventoryQuery,
    label: &str,
    max_rows: usize,
) -> Result<()> {
    require(
        query
            .limit
            .is_none_or(|limit| (1..=MAX_DEVICE_INVENTORY_PAGE).contains(&limit)),
        "InvalidArgument",
        &format!("{label} page limit must be 1 to {MAX_DEVICE_INVENTORY_PAGE}"),
    )?;
    if let Some(cursor) = &query.cursor {
        require(
            (1..max_rows).contains(&usize::from(cursor.offset))
                && valid_digest(&cursor.snapshot_digest),
            "InvalidArgument",
            &format!("invalid {label} cursor"),
        )?;
    }
    Ok(())
}

pub(super) fn inventory_page<'a, Device: Serialize>(
    devices: &'a [Device],
    query: &DeviceInventoryQuery,
    digest: &str,
    label: &str,
) -> Result<(&'a [Device], Option<DeviceInventoryCursor>)> {
    let offset = if let Some(cursor) = &query.cursor {
        require(
            cursor.snapshot_digest == digest,
            "ResyncRequired",
            &format!("{label} changed or cursor scope differs"),
        )?;
        let offset = usize::from(cursor.offset);
        require(
            offset < devices.len(),
            "InvalidArgument",
            &format!("{label} cursor is past the end"),
        )?;
        offset
    } else {
        0
    };
    let mut end = offset;
    let mut bytes = 0;
    let limit = usize::from(query.limit.unwrap_or(MAX_DEVICE_INVENTORY_PAGE));
    for row in devices[offset..].iter().take(limit) {
        bytes += serde_json::to_vec(row)?.len() + 1;
        if bytes > INVENTORY_PAGE_BYTES {
            require(
                end > offset,
                "QuotaExceeded",
                "one device exceeds the inventory page byte limit",
            )?;
            break;
        }
        end += 1;
    }
    let next = if end < devices.len() {
        let offset =
            u16::try_from(end).map_err(|_| err("HostFailure", format!("invalid {label} size")))?;
        Some(DeviceInventoryCursor {
            snapshot_digest: digest.to_owned(),
            offset,
        })
    } else {
        None
    };
    Ok((&devices[offset..end], next))
}
