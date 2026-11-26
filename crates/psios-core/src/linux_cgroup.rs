//! Open kernel cgroup v2 controls below the standard local mount.
use rustix::{
    fd::OwnedFd,
    fs::{Mode, OFlags, ResolveFlags, fstatfs, open, openat2},
};
use std::{fs::File, io, path::Path};

const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;
const DIRECTORY: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const BELOW: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

pub(super) fn root() -> io::Result<OwnedFd> {
    let fd = open("/sys/fs/cgroup", DIRECTORY, Mode::empty()).map_err(io::Error::from)?;
    if fstatfs(&fd).map_err(io::Error::from)?.f_type as u32 != CGROUP2_SUPER_MAGIC {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    Ok(fd)
}

pub(super) fn group(root: OwnedFd, path: &Path) -> io::Result<OwnedFd> {
    if path.as_os_str().is_empty() {
        return Ok(root);
    }
    group_at(&root, path)
}

pub(super) fn group_at(root: &OwnedFd, path: &Path) -> io::Result<OwnedFd> {
    openat2(root, path, DIRECTORY, Mode::empty(), BELOW).map_err(io::Error::from)
}

pub(super) fn control(group: &OwnedFd, name: &str) -> io::Result<File> {
    openat2(
        group,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
        BELOW,
    )
    .map(File::from)
    .map_err(io::Error::from)
}
