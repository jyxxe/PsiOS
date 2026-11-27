//! Observe live cgroup v2 controls for the trusted local Docker adapter.
use crate::{
    Result, err,
    host::Install,
    linux_cgroup,
    node::pressure::{self, CpuLimit, Limit, Reading},
    require,
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::OwnedFd,
    fs::{Mode, OFlags, fstat, open, openat},
    process::{Pid, PidfdFlags, pidfd_open},
};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

const DIRECTORY: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const CONTROL_BYTES: usize = 8192;

pub(super) fn root() -> Result<OwnedFd> {
    linux_cgroup::root().map_err(|error| match error.kind() {
        io::ErrorKind::Unsupported => err(
            "UnsupportedCapability",
            "local cgroup v2 filesystem is not visible",
        ),
        _ => error.into(),
    })
}

fn read(file: File) -> Result<String> {
    let mut text = String::new();
    file.take(CONTROL_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    require(
        text.len() <= CONTROL_BYTES,
        "PreconditionFailed",
        "kernel cgroup observation exceeds limit",
    )?;
    Ok(text)
}

fn membership(process: &OwnedFd, container: &str) -> Result<PathBuf> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let fd = openat(process, "cgroup", flags, Mode::empty()).map_err(io::Error::from)?;
    let current = read(File::from(fd))?;
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

pub(super) struct Process {
    pub(super) pid: i32,
    pub(super) directory: OwnedFd,
    lifetime: OwnedFd,
    path: PathBuf,
}

impl Process {
    pub(super) fn pin(container: &str, state: &Value) -> Result<Self> {
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
        let directory =
            open(format!("/proc/{raw}"), DIRECTORY, Mode::empty()).map_err(io::Error::from)?;
        let path = membership(&directory, container)?;
        let process = Self {
            pid: raw,
            directory,
            lifetime,
            path,
        };
        process.confirm(container)?;
        Ok(process)
    }

    pub(super) fn confirm(&self, container: &str) -> Result<()> {
        let mut watch = [PollFd::new(&self.lifetime, PollFlags::IN)];
        require(
            membership(&self.directory, container)? == self.path
                && poll(&mut watch, Some(&Timespec::default())).map_err(io::Error::from)? == 0,
            "PreconditionFailed",
            "Docker process changed during kernel observation",
        )
    }
}

pub(super) fn observe(
    container: &str,
    state: &Value,
    approval: &Install,
    include_usage: bool,
) -> Result<(Value, Option<Value>)> {
    let process = Process::pin(container, state)?;
    let group = linux_cgroup::group(root()?, &process.path)?;
    let control = |name| read(linux_cgroup::control(&group, name)?);
    let cpu = control("cpu.max")?;
    let mut cpu = cpu.split_whitespace();
    let quota = finite(cpu.next())?;
    let period = finite(cpu.next())?;
    let memory = finite(Some(control("memory.max")?.trim()))?;
    let swap = finite(Some(control("memory.swap.max")?.trim()))?;
    let pids = finite(Some(control("pids.max")?.trim()))?;
    let burst = control("cpu.max.burst")?;
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
    let inode = fstat(&group).map_err(io::Error::from)?;
    let usage = if include_usage {
        let from = pressure::boot_us();
        let snapshot = pressure::cgroup_snapshot(&group)?;
        let current = linux_cgroup::group(root()?, &process.path)?;
        let latest = fstat(&current).map_err(io::Error::from)?;
        let bandwidth = Reading::Observed(CpuLimit {
            quota_us: Limit::Bounded(quota),
            period_us: period,
        });
        let bounded = |value| json!(Reading::Observed(Limit::Bounded(value)));
        require(
            latest.st_dev == inode.st_dev
                && latest.st_ino == inode.st_ino
                && pressure::contains_process(&current, process.pid as u32)?
                && snapshot["cpu"]["configured_limit"] == json!(bandwidth)
                && snapshot["cpu"]["configured_burst_us"] == json!(Reading::Observed(0_u64))
                && snapshot["memory"]["configured_max_bytes"] == bounded(memory)
                && snapshot["memory"]["configured_swap_max_bytes"] == bounded(swap)
                && snapshot["pids"]["configured_max"] == bounded(pids),
            "PreconditionFailed",
            "OCI cgroup identity or limits changed while reading usage",
        )?;
        Some(json!({"from_boot_us":from.to_string(),
            "through_boot_us":pressure::boot_us().to_string(),"cgroup":snapshot}))
    } else {
        None
    };
    process.confirm(container)?;
    let limits = json!({
        "source": "LinuxCgroupV2", "host_pid": process.pid,
        "cgroup_inode": inode.st_ino.to_string(),
        "cpu_quota_us": quota, "cpu_period_us": period, "cpu_burst_us": 0,
        "memory_max_bytes": memory, "swap_max_bytes": swap, "pids_max": pids,
    });
    Ok((limits, usage))
}
