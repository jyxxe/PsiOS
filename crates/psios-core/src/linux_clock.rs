mod source;

use crate::{Result, err, linux_proc, require};
use rustix::fd::OwnedFd;
use rustix::time::{ClockId, clock_gettime};

pub(crate) struct BootClock {
    pub boot_id: String,
    pub millis: i64,
}

impl BootClock {
    pub(crate) fn sample() -> Result<Self> {
        Self::sample_boot(boot_id()?)
    }

    pub(crate) fn sample_with_time_namespace() -> Result<(Self, String)> {
        let root = linux_proc::root()?;
        let thread = linux_proc::thread(&root)?;
        let time_ns = source::time_namespace(&thread)?;
        let sample = Self::sample_boot(source::boot_id(&root)?)?;
        require(
            source::time_namespace(&thread)? == time_ns,
            "PreconditionFailed",
            "Linux time namespace changed during clock sampling",
        )?;
        Ok((sample, time_ns))
    }

    fn sample_boot(boot_id: String) -> Result<Self> {
        let time = clock_gettime(ClockId::Boottime);
        let millis = time
            .tv_sec
            .checked_mul(1000)
            .and_then(|seconds| seconds.checked_add(time.tv_nsec / 1_000_000))
            .filter(|millis| *millis >= 0)
            .ok_or_else(|| {
                err(
                    "StorageUnavailable",
                    "Linux boottime is outside its supported range",
                )
            })?;
        Ok(Self { boot_id, millis })
    }
}

pub(crate) fn boot_id() -> Result<String> {
    boot_id_in(&linux_proc::root()?)
}

pub(crate) fn boot_id_in(root: &OwnedFd) -> Result<String> {
    source::boot_id(root)
}
