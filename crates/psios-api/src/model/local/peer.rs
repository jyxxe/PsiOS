use crate::{ApiError, ApiResult};
use nix::{
    errno::Errno,
    sys::socket::{getsockopt, sockopt::PeerPidfd},
};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    fs::{Mode, OFlags, PROC_SUPER_MAGIC, fstatfs, open, openat},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    os::unix::fs::MetadataExt,
};
use tokio::{net::UnixStream, sync::Semaphore};

const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
static MEASUREMENT_SLOTS: Semaphore = Semaphore::const_new(2);

fn unavailable() -> Box<ApiError> {
    ApiError::new(
        "DependencyUnavailable",
        "could not verify the local inference peer executable",
    )
    .into()
}

pub(super) async fn verify_executable(
    socket: &UnixStream,
    pid: Option<i32>,
    expected: &str,
) -> ApiResult<()> {
    let pid = pid.filter(|pid| *pid > 0).ok_or_else(unavailable)?;
    // Keep a handle to the process from this connection, even if its numeric PID is recycled.
    let pidfd = getsockopt(socket, PeerPidfd).map_err(|error| match error {
        Errno::ENOPROTOOPT | Errno::EINVAL => ApiError::new(
            "UnsupportedCapability",
            "local executable verification requires Linux SO_PEERPIDFD",
        )
        .into(),
        _ => unavailable(),
    })?;
    // A cancelled measurement can finish its current file read. Keep its slot until it exits.
    let slot = MEASUREMENT_SLOTS
        .acquire()
        .await
        .map_err(|_| unavailable())?;
    let measured = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        measure_executable(&pidfd, pid)
    })
    .await
    .map_err(|_| unavailable())?
    .map_err(|_| unavailable())?;
    if measured != expected {
        return Err(ApiError::new(
            "PermissionDenied",
            "local inference peer executable SHA-256 mismatch",
        )
        .into());
    }
    Ok(())
}

fn verify_process(proc: &OwnedFd, pidfd: &OwnedFd, pid: i32) -> io::Result<()> {
    let info_path = format!("self/fdinfo/{}", pidfd.as_raw_fd());
    let info = openat(
        proc,
        info_path,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut details = String::new();
    File::from(info).take(4096).read_to_string(&mut details)?;
    let observed = details
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .and_then(|number| number.trim().parse().ok());
    if observed == Some(pid) {
        Ok(())
    } else {
        Err(io::Error::other(
            "peer process no longer matches its procfs identity",
        ))
    }
}

fn measure_executable(pidfd: &OwnedFd, pid: i32) -> io::Result<String> {
    let proc = open(
        "/proc",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if fstatfs(&proc)?.f_type != PROC_SUPER_MAGIC {
        return Err(io::Error::other(
            "procfs is required to measure the peer executable",
        ));
    }
    verify_process(&proc, pidfd, pid)?;
    let path = format!("{pid}/exe");
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let image = File::from(openat(&proc, path.as_str(), flags, Mode::empty())?);
    let original = image.metadata()?;
    if !original.is_file() || original.len() == 0 || original.len() > MAX_EXECUTABLE_BYTES {
        return Err(io::Error::other(
            "peer executable is outside the measurement size limit",
        ));
    }
    let mut hasher = Sha256::new();
    let read = io::copy(&mut image.take(original.len() + 1), &mut hasher)?;
    // Another exec of the same PID must not substitute a different inode during measurement.
    let current = File::from(openat(
        &proc,
        path,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
    .metadata()?;
    verify_process(&proc, pidfd, pid)?;
    if read != original.len()
        || (original.dev(), original.ino(), original.len())
            != (current.dev(), current.ino(), current.len())
    {
        return Err(io::Error::other(
            "peer executable changed during measurement",
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}
