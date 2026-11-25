use crate::{Config, Result, err, now, require};
use rustix::fs::{StatVfs, StatVfsMountFlags, fstatvfs, statvfs};
use serde_json::{Value, json};
use std::{fs::File, io, os::unix::fs::MetadataExt, path::Path};

#[derive(Clone, Copy, Default)]
struct Reserve {
    bytes: u64,
    inodes: u64,
}

fn sample(path: &Path) -> Result<StatVfs> {
    Ok(statvfs(path).map_err(io::Error::from)?)
}

fn available(fs: &StatVfs) -> u64 {
    fs.f_bavail.saturating_mul(fs.f_frsize)
}

fn free_inodes(fs: &StatVfs) -> Option<u64> {
    // A zero total supplies no usable inode accounting. A zero free count
    // on a filesystem with an observed total is exhaustion.
    (fs.f_files != 0 && fs.f_favail <= fs.f_files).then_some(fs.f_favail)
}

fn describe(path: &Path, reserve: Reserve) -> Result<Value> {
    let fs = sample(path)?;
    let free = available(&fs);
    let inodes = free_inodes(&fs);
    Ok(json!({
        "capacity_bytes": fs.f_blocks.saturating_mul(fs.f_frsize).to_string(),
        "available_bytes": free.to_string(),
        "available_inodes": inodes.map(|free| free.to_string()),
        "inode_accounting": if inodes.is_some() {"Observed"} else {"Unavailable"},
        "read_only": fs.f_flag.contains(StatVfsMountFlags::RDONLY),
        "reserve_bytes": reserve.bytes.to_string(),
        "reserve_inodes": reserve.inodes.to_string(),
        "below_reserve": free < reserve.bytes || inodes.is_some_and(|free| free < reserve.inodes),
    }))
}

fn retryable_exhaustion(message: String) -> crate::Error {
    let mut error = err("QuotaExceeded", message);
    error.0.retryable = true;
    error.0.retry_condition = "SameRequestAfterBackoff".into();
    error
}

impl Config {
    fn filesystems(&self) -> [(&'static str, &Path, Reserve); 2] {
        let reserve = &self.authority_reserve;
        [
            (
                "state",
                &self.state_dir,
                Reserve {
                    bytes: reserve.state_bytes,
                    inodes: reserve.state_inodes,
                },
            ),
            (
                "witness",
                &self.witness_dir,
                Reserve {
                    bytes: reserve.witness_bytes,
                    inodes: reserve.witness_inodes,
                },
            ),
        ]
    }

    pub(crate) fn authority_storage(&self) -> Result<Value> {
        let [(_, state, state_reserve), (_, witness, witness_reserve)] = self.filesystems();
        Ok(json!({
            "state": describe(state, state_reserve)?,
            "witness": describe(witness, witness_reserve)?,
            "observed_at": now(),
        }))
    }

    pub(crate) fn check_admission_space(&self) -> Result<()> {
        for (name, path, reserve) in self.filesystems() {
            if reserve.bytes == 0 && reserve.inodes == 0 {
                continue;
            }
            let fs = sample(path)?;
            if fs.f_flag.contains(StatVfsMountFlags::RDONLY) {
                return Err(err(
                    "StorageUnavailable",
                    format!("authority {name} filesystem is mounted read-only"),
                ));
            }
            if available(&fs) < reserve.bytes {
                return Err(retryable_exhaustion(format!(
                    "authority {name} filesystem below configured free space reserve"
                )));
            }
            if reserve.inodes > 0 {
                let inodes = free_inodes(&fs).ok_or_else(|| {
                    err("UnsupportedCapability", format!("authority {name} filesystem cannot account for its configured inode reserve"))
                })?;
                if inodes < reserve.inodes {
                    return Err(retryable_exhaustion(format!(
                        "authority {name} filesystem below configured free inode reserve"
                    )));
                }
            }
        }
        Ok(())
    }

    fn destination_reserve(&self, destination: &File) -> Result<Reserve> {
        let device = destination.metadata()?.dev();
        let mut reserve = Reserve::default();
        for (_, path, floor) in self.filesystems() {
            if (floor.bytes > reserve.bytes || floor.inodes > reserve.inodes)
                && path.metadata()?.dev() == device
            {
                reserve.bytes = reserve.bytes.max(floor.bytes);
                reserve.inodes = reserve.inodes.max(floor.inodes);
            }
        }
        Ok(reserve)
    }

    pub(crate) fn check_managed_space(
        &self,
        destination: &File,
        file_sizes: impl Iterator<Item = u64>,
        entries: u64,
    ) -> Result<()> {
        check_capacity(
            destination,
            file_sizes,
            entries,
            self.destination_reserve(destination)?,
        )
    }
}

pub(crate) fn check_restore_capacity(
    destination: &File,
    file_sizes: impl Iterator<Item = u64>,
    entries: u64,
    reserve: u64,
) -> Result<()> {
    check_capacity(
        destination,
        file_sizes,
        entries,
        Reserve {
            bytes: reserve,
            inodes: 0,
        },
    )
}

fn check_capacity(
    destination: &File,
    file_sizes: impl Iterator<Item = u64>,
    entries: u64,
    reserve: Reserve,
) -> Result<()> {
    let fs = fstatvfs(destination).map_err(io::Error::from)?;
    require(
        !fs.f_flag.contains(StatVfsMountFlags::RDONLY),
        "StorageUnavailable",
        "destination filesystem is mounted read-only",
    )?;
    let block = fs.f_frsize.max(1);
    let bytes = file_sizes
        .map(|len| len.div_ceil(block).saturating_mul(block))
        .fold(0u64, u64::saturating_add);
    require(
        available(&fs)
            .checked_sub(reserve.bytes)
            .is_some_and(|free| free >= bytes),
        "QuotaExceeded",
        "insufficient destination blocks for allocation and authority reserve",
    )?;
    if let Some(inodes) = free_inodes(&fs) {
        require(
            inodes
                .checked_sub(reserve.inodes)
                .is_some_and(|free| free >= entries),
            "QuotaExceeded",
            "insufficient destination inodes for allocation and authority reserve",
        )?;
    } else {
        require(
            reserve.inodes == 0,
            "UnsupportedCapability",
            "destination filesystem cannot account for its configured inode reserve",
        )?;
    }
    Ok(())
}
