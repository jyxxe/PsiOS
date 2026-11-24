use crate::{Config, Result, err, now};
use rustix::fs::{StatVfs, statvfs};
use serde_json::{Value, json};
use std::{io, path::Path};

fn sample(path: &Path) -> Result<StatVfs> {
    Ok(statvfs(path).map_err(io::Error::from)?)
}

fn available(fs: &StatVfs) -> u64 {
    fs.f_bavail.saturating_mul(fs.f_frsize)
}

fn describe(path: &Path, reserve: u64) -> Result<Value> {
    let fs = sample(path)?;
    let free = available(&fs);
    Ok(json!({
        "capacity_bytes": fs.f_blocks.saturating_mul(fs.f_frsize).to_string(),
        "available_bytes": free.to_string(),
        "available_inodes": fs.f_favail.to_string(),
        "reserve_bytes": reserve.to_string(),
        "below_reserve": free < reserve,
    }))
}

impl Config {
    pub(crate) fn authority_storage(&self) -> Result<Value> {
        Ok(json!({
            "state": describe(&self.state_dir, self.authority_reserve.state_bytes)?,
            "witness": describe(&self.witness_dir, self.authority_reserve.witness_bytes)?,
            "observed_at": now(),
        }))
    }

    pub(crate) fn check_admission_space(&self) -> Result<()> {
        for (name, path, reserve) in [
            ("state", &self.state_dir, self.authority_reserve.state_bytes),
            (
                "witness",
                &self.witness_dir,
                self.authority_reserve.witness_bytes,
            ),
        ] {
            if reserve != 0 && available(&sample(path)?) < reserve {
                let mut e = err(
                    "QuotaExceeded",
                    format!("authority {name} filesystem below configured free space reserve"),
                );
                e.0.retryable = true;
                e.0.retry_condition = "SameRequestAfterBackoff".into();
                return Err(e);
            }
        }
        Ok(())
    }
}
