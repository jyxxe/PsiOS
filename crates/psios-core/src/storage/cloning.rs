//! Copy a detached local volume directly into one privately staged successor.
use super::{
    Capture, CaptureOutput, ModifiedTime, RestoreDestination, capture_root, create_volume,
    require_managed_root_mode, restore_proof, restore_publication,
};
use crate::{
    Result,
    host::{Target, Work},
    label, require,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{File, Permissions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

pub(crate) const TOOL: &str = "volume.clone";
pub(crate) const FORMAT: u32 = super::entry_retirement::FORMAT + 1;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub target: Target,
    pub(crate) name: String,
}

impl Input {
    pub(crate) fn validate(&self) -> Result<()> {
        label(&self.name)
    }
}

pub(super) fn preserve_metadata(directory: &File, mode: u32, time: ModifiedTime) -> Result<()> {
    directory.set_permissions(Permissions::from_mode(mode))?;
    require(
        directory.metadata()?.mode() & 0o7777 == mode,
        "UnsupportedCapability",
        "clone destination did not preserve the ordinary Unix mode",
    )?;
    time.restore(directory)?;
    directory.sync_all()?;
    Ok(())
}

pub(crate) fn run(
    work: &Work,
    checkpoint: impl FnOnce(&Value, &restore_publication::Placement) -> Result<()>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Value> {
    create_volume(
        work,
        Some(|destination: &File| {
            let mut capture = Capture {
                out: Some(CaptureOutput::Tree(RestoreDestination {
                    directory: destination,
                    authority: Some(&work.cfg),
                })),
                entries: Vec::new(),
                total: 0,
                max_bytes: work.cfg.backup_max_bytes,
                check,
            };
            let source = capture_root(work)?;
            let before = source.metadata()?;
            let root_mode = before.mode() & 0o777;
            require_managed_root_mode(root_mode)?;
            let root_time = ModifiedTime::from_metadata(&before);
            capture.walk(&source, Path::new(""), 0)?;
            preserve_metadata(destination, root_mode, root_time)?;
            capture.confirm_source(&capture_root(work)?, &before)?;
            let proof = restore_proof::from_capture(&capture.entries, root_time, root_mode)?;
            Ok(json!(proof))
        }),
        checkpoint,
    )
}
