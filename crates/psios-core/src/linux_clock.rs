use crate::{Result, err};
use rustix::time::{ClockId, clock_gettime};

pub(crate) struct BootClock {
    pub boot_id: String,
    pub millis: i64,
}

impl BootClock {
    pub(crate) fn sample() -> Result<Self> {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let boot_id = uuid::Uuid::parse_str(boot.trim())
            .map_err(|_| err("StorageUnavailable", "invalid Linux boot identity"))?;
        let time = clock_gettime(ClockId::Boottime);
        Ok(Self {
            boot_id: boot_id.to_string(),
            millis: time.tv_sec * 1000 + time.tv_nsec / 1_000_000,
        })
    }
}
