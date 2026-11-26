//! Directory identity checks for the original backup publication and reclamation.
use super::{Doc, File, OFlags, Result, Value, child, err, file_set, filename, require};
use crate::{Config, decode};
use rustix::fs::{AtFlags, statat};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    pub(super) stage_name: String,
    parent: [u64; 2],
    pub(super) directory: [u64; 2],
    pub(super) file_set_digest: String,
}

impl Placement {
    pub(in crate::storage) fn staged(stage: &super::staging::Stage) -> Result<Self> {
        let stage_name = stage
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| super::staging::is_stage_name(name))
            .ok_or_else(|| err("RecoveryRequired", "backup stage name is invalid"))?;
        let (device, inode) = stage.identity()?;
        let parent = stage.parent().metadata()?;
        Ok(Self {
            stage_name: stage_name.into(),
            parent: [parent.dev(), parent.ino()],
            directory: [device, inode],
            file_set_digest: file_set(stage.directory())?.0,
        })
    }

    pub(super) fn from_checkpoint(checkpoint: &Doc) -> Result<Option<Self>> {
        let Some(value) = checkpoint.body.get("placement") else {
            return Ok(None);
        };
        let placement: Self = decode(value)?;
        require(
            super::staging::is_stage_name(&placement.stage_name)
                && super::file_integrity::sha256_format(&placement.file_set_digest)
                && checkpoint.body["directory"] == serde_json::json!(placement.directory),
            "RecoveryRequired",
            "backup stage placement does not match its checkpoint",
        )?;
        Ok(Some(placement))
    }

    pub(super) fn parent(&self, config: &Config) -> Result<File> {
        let parent = super::open_backup_parent(config)?;
        require(
            private_identity(&parent, self.parent)?,
            "PreconditionFailed",
            "backup parent differs from the checkpoint",
        )?;
        Ok(parent)
    }

    pub(super) fn directory(&self, parent: &File, name: &str) -> Result<File> {
        let directory = child(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
        require(
            private_identity(&directory, self.directory)?,
            "PreconditionFailed",
            "backup directory differs from the checkpoint",
        )?;
        Ok(directory)
    }
}

fn private_identity(file: &File, expected: [u64; 2]) -> Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_dir()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o077 == 0
        && [metadata.dev(), metadata.ino()] == expected)
}

pub(super) fn present(parent: &File, name: &str) -> Result<bool> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

pub(super) fn backup_id(receipt: &Value) -> Result<&str> {
    let id = receipt["id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "checkpointed backup has no identity"))?;
    filename(id)?;
    Ok(id)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Publication {
    Published,
    NotApplied,
}

impl Publication {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Published => "Published",
            Self::NotApplied => "NotApplied",
        }
    }
}

pub(super) fn open_candidate(
    config: &Config,
    checkpoint: &Doc,
    placement: Option<&Placement>,
) -> Result<(Publication, File, File)> {
    let id = backup_id(&checkpoint.body["receipt"])?;
    let Some(placement) = placement else {
        let (parent, root) = super::open_backup(config, id)?;
        let metadata = root.metadata()?;
        require(
            checkpoint.body["directory"] == serde_json::json!([metadata.dev(), metadata.ino()]),
            "PreconditionFailed",
            "published backup directory differs from the checkpoint",
        )?;
        return Ok((Publication::Published, parent, root));
    };
    let parent = placement.parent(config)?;
    let stage = &placement.stage_name;
    let (publication, name, alternate) = match (present(&parent, id)?, present(&parent, stage)?) {
        (true, false) => (Publication::Published, id, stage.as_str()),
        (false, true) => (Publication::NotApplied, stage.as_str(), id),
        _ => {
            return Err(err(
                "PreconditionFailed",
                "neither placement uniquely identifies the original backup",
            ));
        }
    };
    let directory = placement.directory(&parent, name)?;
    require(
        !present(&parent, alternate)?,
        "PreconditionFailed",
        "competing backup publication placement is present",
    )?;
    Ok((publication, parent, directory))
}
