//! Anchored reads of kernel procfs without following foreign mounts or links.
use rustix::{
    fd::OwnedFd,
    fs::{FileType, Mode, OFlags, ResolveFlags, fstat, fstatfs, open, openat2, readlinkat},
};
use std::{fs::File, io};

pub(crate) const PROC_MAGIC: u32 = 0x9fa0;
pub(crate) const BENEATH: ResolveFlags = ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS);
const SAME_MOUNT: ResolveFlags = BENEATH.union(ResolveFlags::NO_XDEV);

pub(crate) fn root() -> io::Result<OwnedFd> {
    let root = open(
        "/proc",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if fstatfs(&root)?.f_type as u32 != PROC_MAGIC {
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(root)
}

pub(crate) fn directory(root: &OwnedFd, path: &str) -> io::Result<OwnedFd> {
    openat2(
        root,
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        SAME_MOUNT,
    )
    .map_err(io::Error::from)
}

pub(crate) fn file(root: &OwnedFd, path: &str) -> io::Result<File> {
    let fd = openat2(
        root,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        SAME_MOUNT,
    )?;
    if FileType::from_raw_mode(fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(File::from(fd))
}

pub(crate) fn thread(root: &OwnedFd) -> io::Result<OwnedFd> {
    // Resolve only the genuine kernel link, then pin the numeric path below
    // the verified mount. It can reflect a different PID view from gettid().
    let link = readlinkat(root, "thread-self", Vec::new())?;
    let path = link.to_str().map_err(|_| io::ErrorKind::InvalidData)?;
    let (process, thread) = path
        .split_once("/task/")
        .ok_or(io::ErrorKind::InvalidData)?;
    if ![process, thread].into_iter().all(|id| {
        id.parse::<u32>()
            .is_ok_and(|n| n > 0 && n.to_string() == id)
    }) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    directory(root, path)
}
