//! Accept boot and time-namespace labels only from their verified Linux sources.
use crate::{
    Result, err, linux_proc,
    node::mounts::{self, BootIdMount},
    require,
};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{
        AtFlags, FileType, Mode, OFlags, StatVfsMountFlags, StatxFlags, fstat, fstatfs, fstatvfs,
        major, minor, openat, openat2, readlinkat, statx,
    },
    io::Errno,
};
use std::{
    fs::File,
    io::{self, Read},
};

const BOOT_ID: &str = "sys/kernel/random/boot_id";
const BOOT_BYTES: u64 = 37;
const TMPFS_MAGIC: u32 = 0x0102_1994;
const NSFS_MAGIC: u32 = 0x6e73_6673;

pub(super) fn boot_id(root: &OwnedFd) -> Result<String> {
    let file = match linux_proc::file(root, BOOT_ID) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
            mounted_boot_file(root)?
        }
        Err(error) if error.raw_os_error() == Some(Errno::NOSYS.raw_os_error()) => {
            return Err(err(
                "UnsupportedCapability",
                "kernel boot file resolution is unavailable",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let mut value = String::with_capacity(BOOT_BYTES as usize + 1);
    file.take(BOOT_BYTES + 1).read_to_string(&mut value)?;
    let text = value.strip_suffix('\n').unwrap_or(&value);
    require(
        value.len() as u64 <= BOOT_BYTES
            && uuid::Uuid::parse_str(text).is_ok_and(|id| !id.is_nil() && id.to_string() == text),
        "StorageUnavailable",
        "invalid Linux boot identity",
    )?;
    Ok(text.into())
}

fn mount_id(fd: &OwnedFd) -> Result<u64> {
    let id = statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID).map_err(io::Error::from)?;
    require(
        id.stx_mask & StatxFlags::MNT_ID.bits() != 0 && id.stx_mnt_id != 0,
        "UnsupportedCapability",
        "kernel boot file mount identity is unavailable",
    )?;
    Ok(id.stx_mnt_id)
}

fn read_only(fd: &OwnedFd) -> Result<bool> {
    Ok(fstatvfs(fd)
        .map_err(io::Error::from)?
        .f_flag
        .contains(StatVfsMountFlags::RDONLY))
}

fn mounted_boot_file(root: &OwnedFd) -> Result<File> {
    // A foreign FIFO or device may occupy this name. Pin metadata first and
    // only open a data descriptor after proving an allowed mount profile.
    let pinned = openat2(
        root,
        BOOT_ID,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        linux_proc::BENEATH,
    )
    .map_err(io::Error::from)?;
    let observed = fstat(&pinned).map_err(io::Error::from)?;
    require(
        FileType::from_raw_mode(observed.st_mode) == FileType::RegularFile && read_only(&pinned)?,
        "UnsupportedCapability",
        "Linux boot ID requires a read-only regular file mount",
    )?;
    let id = mount_id(&pinned)?;
    let proc_info = fstat(root).map_err(io::Error::from)?;
    let thread = linux_proc::thread(root)?;
    let profile = mounts::boot_id_mount(
        linux_proc::file(&thread, "mountinfo")?,
        id,
        (major(observed.st_dev), minor(observed.st_dev)),
        (major(proc_info.st_dev), minor(proc_info.st_dev)),
    )?;
    let magic = fstatfs(&pinned).map_err(io::Error::from)?.f_type as u32;
    let valid = match profile {
        BootIdMount::NativeProc => magic == linux_proc::PROC_MAGIC,
        BootIdMount::LocalLxc => {
            magic == TMPFS_MAGIC
                && observed.st_uid == 0
                && observed.st_gid == 0
                && observed.st_nlink == 1
                && observed.st_mode & 0o7777 == 0o444
                && observed.st_size == BOOT_BYTES as i64
        }
    };
    require(
        valid,
        "UnsupportedCapability",
        "Linux boot ID mount does not match its source profile",
    )?;

    // Only the kernel's own descriptor link may be followed here. It reopens
    // the inode already verified above, even if the visible path is replaced.
    let descriptors = linux_proc::directory(&thread, "fd")?;
    let readable = openat(
        &descriptors,
        pinned.as_raw_fd().to_string(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let fresh = fstat(&readable).map_err(io::Error::from)?;
    require(
        fresh.st_dev == observed.st_dev
            && fresh.st_ino == observed.st_ino
            && fresh.st_mode == observed.st_mode
            && fresh.st_uid == observed.st_uid
            && fresh.st_gid == observed.st_gid
            && fresh.st_nlink == observed.st_nlink
            && fresh.st_size == observed.st_size
            && mount_id(&readable)? == id
            && read_only(&readable)?,
        "PreconditionFailed",
        "Linux boot ID file changed while opening its data descriptor",
    )?;
    Ok(File::from(readable))
}

pub(super) fn time_namespace(thread: &OwnedFd) -> Result<String> {
    let namespaces = linux_proc::directory(thread, "ns")?;
    let label = match readlinkat(&namespaces, "time", Vec::new()) {
        Ok(label) => label,
        // This is absence inside an unmasked genuine procfs namespace directory.
        Err(Errno::NOENT) => return Ok("unsupported".into()),
        Err(error) => return Err(io::Error::from(error).into()),
    };
    let inode = label
        .to_str()
        .ok()
        .and_then(|label| label.strip_prefix("time:["))
        .and_then(|label| label.strip_suffix(']'))
        .and_then(|inode| {
            inode
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0 && n.to_string() == inode)
        })
        .ok_or_else(|| {
            err(
                "UnsupportedCapability",
                "invalid Linux time namespace label",
            )
        })?;
    let namespace = openat(
        &namespaces,
        "time",
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    require(
        fstatfs(&namespace).map_err(io::Error::from)?.f_type as u32 == NSFS_MAGIC
            && fstat(&namespace).map_err(io::Error::from)?.st_ino == inode,
        "UnsupportedCapability",
        "Linux time namespace does not match the calling thread",
    )?;
    Ok(inode.to_string())
}
