//! Inspect a detached volume with the backup capture's source checks and no payload I/O.

use super::{
    Capture, CaptureMode, MAX_CAPTURE_DEPTH, MAX_ENTRIES, capture_root, checked_root_metadata,
    require_managed_root_mode,
};
use crate::{Result, apps, host::Work, now};
use serde_json::{Value, json};
use std::{os::unix::fs::MetadataExt, path::Path};

pub(crate) const TOOL: &str = "backup.estimate";
pub(crate) const FORMAT: u32 = apps::health::STARTUP_FORMAT + 1;

pub(crate) fn run(work: &Work, check: &mut impl FnMut() -> Result<()>) -> Result<Value> {
    check()?;
    let root = capture_root(work)?;
    let before = checked_root_metadata(&root, "UnsupportedCapability")?;
    require_managed_root_mode(before.mode() & 0o777)?;
    let mut scan = Capture {
        mode: CaptureMode::MetadataOnly,
        entries: Vec::new(),
        total: 0,
        max_bytes: u64::MAX,
        check,
    };
    scan.walk(&root, Path::new(""), 0)?;
    let current = capture_root(work)?;
    checked_root_metadata(&current, "Conflict")?;
    scan.confirm_source(&current, &before)?;
    let directories = scan
        .entries
        .iter()
        .filter(|(_, meta)| meta.is_dir())
        .count();
    Ok(json!({
        "source_ref": work.target.reference(),
        "data_incarnation": work.target.body["data_incarnation"],
        "logical_bytes": scan.total.to_string(),
        "entries": scan.entries.len(),
        "files": scan.entries.len() - directories,
        "directories": directories,
        "fits_payload_limit": scan.total <= work.cfg.backup_max_bytes,
        "profile": {
            "payload_limit_bytes": work.cfg.backup_max_bytes.to_string(),
            "entry_limit": MAX_ENTRIES,
            "directory_depth_limit": MAX_CAPTURE_DEPTH,
            "capture_timeout_seconds": work.cfg.backup_capture_timeout_seconds,
        },
        "observed_at": now(),
    }))
}
