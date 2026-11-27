//! Inspect a detached volume with the backup capture's source checks and no payload I/O.

use super::{
    Capture, CaptureMode, MAX_CAPTURE_DEPTH, MAX_ENTRIES, capture_root, require_managed_root_mode,
    root_attribute_profile,
};
use crate::{Result, apps, host::Work, now};
use serde_json::{Value, json};
use std::{os::unix::fs::MetadataExt, path::Path};

pub(crate) const TOOL: &str = "backup.estimate";
pub(crate) const FORMAT: u32 = apps::health::STARTUP_FORMAT + 1;

pub(crate) fn run(work: &Work, check: &mut impl FnMut() -> Result<()>) -> Result<Value> {
    check()?;
    let root = capture_root(work)?;
    let preserve = work.cfg.backup_user_xattrs;
    let (before, root_attributes) =
        root_attribute_profile(&root, "UnsupportedCapability", preserve)?;
    require_managed_root_mode(before.mode() & 0o777)?;
    let mut scan = Capture::new(CaptureMode::MetadataOnly, u64::MAX, MAX_ENTRIES, check)
        .with_root_attributes(root_attributes);
    scan.walk(&root, Path::new(""), 0)?;
    let current = capture_root(work)?;
    root_attribute_profile(&current, "Conflict", preserve)?;
    scan.confirm_source(&current, &before)?;
    let directories = scan
        .entries
        .iter()
        .filter(|(_, meta)| meta.is_dir())
        .count();
    let mut result = json!({
        "source_ref": work.target.reference(),
        "data_incarnation": work.target.body["data_incarnation"],
        "logical_bytes": scan.total.to_string(),
        "entries": scan.entries.len(),
        "files": scan.entries.len() - directories,
        "directories": directories,
        "fits_payload_limit": scan.total <= work.cfg.backup_max_bytes,
        "fits_entry_limit": scan.entries.len() <= work.cfg.backup_max_entries,
        "profile": {
            "payload_limit_bytes": work.cfg.backup_max_bytes.to_string(),
            "entry_limit": work.cfg.backup_max_entries,
            "entry_hard_limit": MAX_ENTRIES,
            "directory_depth_limit": MAX_CAPTURE_DEPTH,
            "capture_timeout_seconds": work.cfg.backup_capture_timeout_seconds,
        },
        "observed_at": now(),
    });
    if preserve {
        result["user_xattr_metadata_bytes"] = json!(scan.xattr_bytes);
    }
    Ok(result)
}
