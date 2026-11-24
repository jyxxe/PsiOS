use crate::{Result, db::Db, linux_clock::BootClock};
use rusqlite::params;
use std::{fs, io, os::unix::fs::MetadataExt};

pub(crate) struct Clock {
    pub boot: BootClock,
    pub time_ns: String,
}

impl Clock {
    pub fn sample() -> Result<Self> {
        let time_ns = match fs::metadata("/proc/self/ns/time") {
            Ok(metadata) => metadata.ino().to_string(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => "unsupported".into(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            boot: BootClock::sample()?,
            time_ns,
        })
    }

    pub(super) fn register(&self, db: &Db<'_>, resource: &str, ttl_seconds: u32) -> Result<()> {
        let deadline_ms = self
            .boot
            .millis
            .saturating_add(i64::from(ttl_seconds) * 1000);
        db.c.execute(
            "INSERT INTO authorization_deadlines(resource,boot_id,time_ns,deadline_boot_ms) VALUES(?1,?2,?3,?4)",
            params![resource, self.boot.boot_id, self.time_ns, deadline_ms],
        )?;
        Ok(())
    }

    pub(super) fn expired(&self, db: &Db<'_>, resource: &str, utc: &str) -> Result<bool> {
        if crate::expired(utc)? {
            return Ok(true);
        }
        Ok(db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM authorization_deadlines WHERE resource=?1 AND boot_id=?2
             AND (time_ns!=?3 OR deadline_boot_ms<=?4))",
            params![resource, self.boot.boot_id, self.time_ns, self.boot.millis],
            |row| row.get(0),
        )?)
    }
}
