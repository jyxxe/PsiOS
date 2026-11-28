use rustix::{
    fd::OwnedFd,
    fs::{Mode, OFlags, ResolveFlags, fstatfs, openat2},
};
use std::{
    fs::File,
    io::{self, Read},
};

const SYSFS_MAGIC: u32 = 0x6265_6572;
const ATTRIBUTE_BYTES: u64 = 4096;

pub(super) const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);
pub(super) const BELOW: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

pub(super) fn is_sysfs(dir: &OwnedFd) -> io::Result<bool> {
    Ok(fstatfs(dir).map_err(io::Error::from)?.f_type as u32 == SYSFS_MAGIC)
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
