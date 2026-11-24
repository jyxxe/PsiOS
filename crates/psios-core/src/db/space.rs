use crate::{Config, Result, err, now, require};
use rustix::fs::{StatVfs, fstatvfs, statvfs};
use serde_json::{Value, json};
use std::{fs::File, io, os::unix::fs::MetadataExt, path::Path};

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

    pub(crate) fn check_restore_space(
        &self,
        destination: &File,
        file_sizes: impl Iterator<Item = u64>,
        entries: u64,
    ) -> Result<()> {
        let device = destination.metadata()?.dev();
        let mut reserve = 0;
        for (path, floor) in [
            (&self.state_dir, self.authority_reserve.state_bytes),
            (&self.witness_dir, self.authority_reserve.witness_bytes),
        ] {
            if floor > reserve && path.metadata()?.dev() == device {
                reserve = floor;
            }
        }
        check_restore_capacity(destination, file_sizes, entries, reserve)
    }
}

pub(crate) fn check_restore_capacity(
    destination: &File,
    file_sizes: impl Iterator<Item = u64>,
    entries: u64,
    reserve: u64,
) -> Result<()> {
    let fs = fstatvfs(destination).map_err(io::Error::from)?;
    let block = fs.f_frsize.max(1);
    let bytes = file_sizes
        .map(|len| len.div_ceil(block).saturating_mul(block))
        .fold(0u64, u64::saturating_add);
    require(
        available(&fs) >= reserve.saturating_add(bytes),
        "QuotaExceeded",
        "insufficient destination blocks for backup restore and authority reserve",
    )?;
    require(
        fs.f_files == 0 || entries <= fs.f_favail,
        "QuotaExceeded",
        "insufficient destination inodes for backup restore",
    )
}
