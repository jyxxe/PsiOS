use crate::{Result, db::Db, err, linux_clock::BootClock, require};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
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
        self.write(db, resource, deadline_ms)
    }

    pub(crate) fn register_until(
        &self,
        db: &Db<'_>,
        resource: &str,
        utc: &str,
        inherited_from: Option<&str>,
    ) -> Result<()> {
        let at = chrono::DateTime::parse_from_rfc3339(utc)
            .map_err(|_| err("InvalidArgument", "invalid UTC deadline"))?;
        let remaining_ms = (at.to_utc() - chrono::Utc::now()).num_milliseconds();
        let mut deadline_ms = self.boot.millis.saturating_add(remaining_ms);
        if let Some(source) = inherited_from {
            let prior: Option<(String, i64)> = db.c.query_row(
                "SELECT time_ns,deadline_boot_ms FROM authorization_deadlines WHERE resource=?1 AND boot_id=?2",
                params![source, self.boot.boot_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            if let Some((time_ns, inherited_deadline)) = prior {
                require(
                    time_ns == self.time_ns,
                    "DeadlineExceeded",
                    "inherited deadline is outside the current time namespace",
                )?;
                deadline_ms = deadline_ms.min(inherited_deadline);
            }
        }
        require(
            deadline_ms > self.boot.millis,
            "DeadlineExceeded",
            "operation deadline passed",
        )?;
        self.write(db, resource, deadline_ms)
    }

    fn write(&self, db: &Db<'_>, resource: &str, deadline_ms: i64) -> Result<()> {
        db.c.execute(
            "INSERT INTO authorization_deadlines(resource,boot_id,time_ns,deadline_boot_ms) VALUES(?1,?2,?3,?4)",
            params![resource, self.boot.boot_id, self.time_ns, deadline_ms],
        )?;
        Ok(())
    }

    pub(crate) fn expired(&self, db: &Db<'_>, resource: &str, utc: &str) -> Result<bool> {
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

    pub(crate) fn bounded_utc_ms(
        &self,
        db: &Db<'_>,
        resource: &str,
        utc: &Value,
        sampled_utc_ms: i64,
    ) -> Result<i64> {
        let absolute = utc
            .as_str()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "stored authorization deadline is invalid",
                )
            })?
            .timestamp_millis();
        let elapsed: Option<(String, i64)> =
            db.c.query_row(
                "SELECT time_ns,deadline_boot_ms FROM authorization_deadlines
                 WHERE resource=?1 AND boot_id=?2",
                params![resource, self.boot.boot_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match elapsed {
            Some((namespace, _)) if namespace != self.time_ns => absolute.min(sampled_utc_ms),
            Some((_, cutoff)) => {
                absolute.min(sampled_utc_ms.saturating_add(cutoff.saturating_sub(self.boot.millis)))
            }
            None => absolute,
        })
    }
}
