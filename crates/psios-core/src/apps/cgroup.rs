//! Observe live cgroup v2 controls for the trusted local Docker adapter.
use crate::{Result, err, host::Install, require};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::OwnedFd,
    fs::{Mode, OFlags, ResolveFlags, fstat, fstatfs, open, openat, openat2},
    process::{Pid, PidfdFlags, pidfd_open},
};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

const ROOT: &str = "/sys/fs/cgroup";
const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;
const DIRECTORY: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const CONTROL_BYTES: usize = 8192;

pub(super) fn root() -> Result<OwnedFd> {
    let fd = open(ROOT, DIRECTORY, Mode::empty()).map_err(io::Error::from)?;
    require(
        fstatfs(&fd).map_err(io::Error::from)?.f_type as u32 == CGROUP2_SUPER_MAGIC,
        "UnsupportedCapability",
        "local cgroup v2 filesystem is not visible",
    )?;
    Ok(fd)
}

fn read(dir: &OwnedFd, name: &str) -> Result<String> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let fd = openat(dir, name, flags, Mode::empty()).map_err(io::Error::from)?;
    let mut text = String::new();
    File::from(fd)
        .take(CONTROL_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    require(
        text.len() <= CONTROL_BYTES,
        "PreconditionFailed",
        "kernel cgroup observation exceeds limit",
    )?;
    Ok(text)
}

fn membership(process: &OwnedFd, container: &str) -> Result<PathBuf> {
    let current = read(process, "cgroup")?;
    let relative = current
        .strip_prefix("0::/")
        .and_then(|line| line.strip_suffix('\n'))
        .filter(|line| !line.contains('\n'))
        .map(Path::new)
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "process has no unified cgroup membership",
            )
        })?;
    let systemd = format!("docker-{container}.scope");
    let leaf = relative.file_name().and_then(|name| name.to_str());
    let docker = leaf == Some(container)
        && relative
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "docker");
    require(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            && (leaf == Some(systemd.as_str()) || docker),
        "PreconditionFailed",
        "process cgroup does not match the Docker container",
    )?;
    Ok(relative.to_owned())
}

fn finite(value: Option<&str>) -> Result<u64> {
    value.and_then(|n| n.parse().ok()).ok_or_else(|| {
        err(
            "PreconditionFailed",
            "kernel cgroup limit missing or unbounded",
        )
    })
}

pub(super) fn observe(container: &str, state: &Value, approval: &Install) -> Result<Value> {
    let raw = state["Pid"]
        .as_i64()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "Docker did not report a live host PID",
            )
        })?;
    let pid = Pid::from_raw(raw).expect("positive host PID");
    let lifetime = pidfd_open(pid, PidfdFlags::empty()).map_err(io::Error::from)?;
    let proc = open(format!("/proc/{raw}"), DIRECTORY, Mode::empty()).map_err(io::Error::from)?;
    let path = membership(&proc, container)?;
    let group = openat2(
        root()?,
        &path,
        DIRECTORY,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map_err(io::Error::from)?;
    let cpu = read(&group, "cpu.max")?;
    let mut cpu = cpu.split_whitespace();
    let quota = finite(cpu.next())?;
    let period = finite(cpu.next())?;
    let memory = finite(Some(read(&group, "memory.max")?.trim()))?;
    let swap = finite(Some(read(&group, "memory.swap.max")?.trim()))?;
    let pids = finite(Some(read(&group, "pids.max")?.trim()))?;
    let burst = read(&group, "cpu.max.burst")?;
    require(
        cpu.next().is_none()
            && quota > 0
            && period > 0
            && burst.trim() == "0"
            && u128::from(quota) * 1000 <= u128::from(period) * u128::from(approval.cpu_millicores)
            && memory > 0
            && memory <= approval.memory_max_bytes
            && swap == 0
            && pids > 0
            && pids <= u64::from(approval.pids_max),
        "PreconditionFailed",
        "live kernel cgroup limits exceed the approved resource profile",
    )?;
    let mut watch = [PollFd::new(&lifetime, PollFlags::IN)];
    require(
        membership(&proc, container)? == path
            && poll(&mut watch, Some(&Timespec::default())).map_err(io::Error::from)? == 0,
        "PreconditionFailed",
        "Docker process changed during kernel observation",
    )?;
    Ok(json!({
        "source": "LinuxCgroupV2", "host_pid": raw,
        "cgroup_inode": fstat(&group).map_err(io::Error::from)?.st_ino.to_string(),
        "cpu_quota_us": quota, "cpu_period_us": period, "cpu_burst_us": 0,
        "memory_max_bytes": memory, "swap_max_bytes": swap, "pids_max": pids,
    }))
}
