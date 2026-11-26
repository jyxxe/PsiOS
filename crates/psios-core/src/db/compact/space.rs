//! Account for the peak space needed to rebuild and checkpoint the same database.
use crate::{Config, Result, err, require};
use std::{fs::File, os::unix::fs::MetadataExt};

// SQLite's WAL uses one 32-byte header and a 24-byte header per frame.
const WAL_HEADER: u64 = 32;
const WAL_FRAME_HEADER: u64 = 24;
// The Unix VFS allocates its WAL index in 32 KiB regions for approximately
// 4096 frames per region. The first region has fewer slots due to its header.
const WAL_INDEX_REGION: u64 = 32 * 1024;
const WAL_INDEX_FRAMES: u64 = 4096;
const FILE_METADATA: u64 = 64 * 1024;
const STATE_ENTRIES: u64 = 2; // WAL and shared-memory sidecar
const TEMP_ENTRIES: u64 = 2; // Rebuilt database and temporary rollback journal

fn overflow() -> crate::Error {
    err(
        "QuotaExceeded",
        "SQLite compaction space estimate exceeds its size range",
    )
}

fn wal_index(frames: u64) -> Result<u64> {
    frames
        .div_ceil(WAL_INDEX_FRAMES)
        .checked_add(1)
        .and_then(|regions| regions.checked_mul(WAL_INDEX_REGION))
        .ok_or_else(overflow)
}

fn capacity(
    cfg: &Config,
    directory: &File,
    subject: &str,
    file_sizes: impl Iterator<Item = u64>,
    entries: u64,
) -> Result<()> {
    cfg.check_managed_space(directory, file_sizes, entries)
        .map_err(|mut error| {
            error.0.message = format!("SQLite compaction {subject}: {}", error.0.message);
            error
        })
}

/// Call before opening the original read-write connection. Holding the
/// authority lock prevents competing PsiOS writes. Statvfs is an admission
/// sample, so SQLite still handles allocation errors during the operation.
pub(super) fn check(
    cfg: &Config,
    state: &File,
    temp: &File,
    main_bytes: u64,
    wal_bytes: u64,
    page_size: u64,
    vacuum: bool,
) -> Result<()> {
    if !vacuum && wal_bytes == 0 {
        return Ok(());
    }
    require(
        (512..=65_536).contains(&page_size) && page_size.is_power_of_two(),
        "RecoveryRequired",
        "unexpected SQLite page size for compaction",
    )?;
    let state_dir = state.metadata()?;
    let configured = cfg.state_dir.metadata()?;
    require(
        state_dir.is_dir()
            && state_dir.dev() == configured.dev()
            && state_dir.ino() == configured.ino(),
        "RecoveryRequired",
        "SQLite compaction state directory changed",
    )?;
    cfg.check_admission_space()?;
    let frame_bytes = page_size + WAL_FRAME_HEADER;
    let old_frames = wal_bytes.div_ceil(frame_bytes);
    if !vacuum {
        // Checkpointing can allocate main-file blocks even without VACUUM.
        return capacity(
            cfg,
            state,
            "state filesystem",
            [wal_bytes, wal_index(old_frames)?, FILE_METADATA].into_iter(),
            STATE_ENTRIES,
        );
    }

    let temp_dir = temp.metadata()?;
    require(
        temp_dir.is_dir(),
        "RecoveryRequired",
        "SQLite temporary storage changed",
    )?;

    // The existing WAL can contain pages above the on-disk main-file size.
    let pages = main_bytes
        .checked_add(wal_bytes)
        .ok_or_else(overflow)?
        .div_ceil(page_size);
    let rebuilt = pages.checked_mul(page_size).ok_or_else(overflow)?;
    // Allow extra bookkeeping frames in addition to the rebuilt page set.
    let new_frames = pages.checked_add(16).ok_or_else(overflow)?;
    let new_wal = new_frames
        .checked_mul(frame_bytes)
        .and_then(|bytes| bytes.checked_add(WAL_HEADER))
        .ok_or_else(overflow)?;
    let index = wal_index(old_frames.checked_add(new_frames).ok_or_else(overflow)?)?;

    let temp_files = [rebuilt, FILE_METADATA];
    // Sample the actual VFS directory even on the same device: mount flags
    // and filesystem quota reporting can depend on the selected directory.
    capacity(
        cfg,
        temp,
        "temporary filesystem",
        temp_files.into_iter(),
        TEMP_ENTRIES,
    )?;
    let shared = temp_dir.dev() == state_dir.dev();
    // The sealed rebuild uses WAL with autocheckpoint disabled. The temporary
    // database is closed before the explicit checkpoint, which can allocate
    // up to the rebuilt extent again (including on copy-on-write filesystems).
    // Reuse the same device's space between these two phases.
    let temporary_metadata = shared.then_some(FILE_METADATA);
    let disk_files = [new_wal, index, rebuilt, FILE_METADATA]
        .into_iter()
        .chain(temporary_metadata);
    capacity(
        cfg,
        state,
        "state filesystem",
        disk_files,
        STATE_ENTRIES + if shared { TEMP_ENTRIES } else { 0 },
    )
}
