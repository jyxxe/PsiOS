//! Pin the exact live process whose cgroup is named by a trusted host adapter.
use super::{Identity, Proc, STAT_LIMIT, Sample, proc_directory, read_at, stat_error};
use crate::{Result, err, require};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::OwnedFd,
    fs::readlinkat,
    process::{Pid, PidfdFlags, pidfd_open},
};
use std::io;

const MEMBERSHIP_BYTES: u64 = 8192;

pub(in crate::node) struct Pinned {
    directory: OwnedFd,
    lifetime: OwnedFd,
    reference: Identity,
}

fn start_ticks(directory: &OwnedFd, pid: u32) -> Result<u64> {
    let raw = read_at(directory, "stat", STAT_LIMIT)?;
    Sample::parse(&raw, pid)
        .filter(|sample| !matches!(sample.state, "Z" | "X" | "x"))
        .map(|sample| sample.start_ticks)
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "service main process has no live identity",
            )
        })
}

impl Pinned {
    pub(in crate::node) fn pin(node: &str, pid: u32) -> Result<Self> {
        let process = Proc::open(node)?;
        let proc_self = readlinkat(&process.root, "self", Vec::new()).map_err(io::Error::from)?;
        require(
            proc_self.as_bytes() == std::process::id().to_string().as_bytes(),
            "UnsupportedCapability",
            "local procfs and PID syscall views differ",
        )?;
        let target = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| err("PreconditionFailed", "invalid service main process"))?;
        let lifetime = pidfd_open(target, PidfdFlags::empty()).map_err(|error| {
            let error = io::Error::from(error);
            if error.kind() == io::ErrorKind::Unsupported {
                err(
                    "UnsupportedCapability",
                    "kernel PID descriptors are unavailable",
                )
            } else {
                stat_error(error)
            }
        })?;
        let directory = proc_directory(&process.root, &pid.to_string()).map_err(stat_error)?;
        let reference = process.identity(pid, start_ticks(&directory, pid)?);
        Ok(Self {
            directory,
            lifetime,
            reference,
        })
    }

    pub(in crate::node) fn reference(&self) -> &Identity {
        &self.reference
    }

    pub(in crate::node) fn confirm(&self, expected: &str) -> Result<()> {
        let membership = read_at(&self.directory, "cgroup", MEMBERSHIP_BYTES)?;
        let mut lifetime = [PollFd::new(&self.lifetime, PollFlags::IN)];
        require(
            membership
                .strip_prefix("0::")
                .and_then(|line| line.strip_suffix('\n'))
                == Some(expected)
                && start_ticks(&self.directory, self.reference.pid)?.to_string()
                    == self.reference.start_ticks
                && poll(&mut lifetime, Some(&Timespec::default())).map_err(io::Error::from)? == 0,
            "PreconditionFailed",
            "service main process moved or exited during observation",
        )
    }
}
