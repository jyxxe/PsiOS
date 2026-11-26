//! Prove durable effects for the original volume write or append without writing again.
use super::{child, file_integrity, filename, files, regular, unknown};
use crate::{Config, Error, Result, db::Doc, err, host::Action, now, require};
use psios_api::sha256;
use rustix::fs::OFlags;
use serde_json::{Value, json};
use std::{fs::File, os::unix::fs::FileExt};

fn parent(cfg: &Config, volume: &Doc, path: &str) -> Result<(File, String)> {
    let root = file_integrity::volume_root(cfg, volume)?;
    files::parent(&root, path)
}

fn unverified(error: Error) -> Error {
    err(
        "OutcomeUnknown",
        format!("cannot verify the approved volume file: {}", error.0.code),
    )
}

pub(crate) struct Probe {
    config: Config,
    volume: Doc,
    operation_ref: Value,
    path: String,
    offset: Option<u64>,
    expected: Vec<u8>,
}

impl Probe {
    pub(crate) fn new(cfg: &Config, volume: Doc, operation: &Doc, action: Action) -> Result<Self> {
        let (path, offset, expected) = match action {
            Action::Write(write) => {
                filename(&write.name)?;
                (write.name, None, write.content.into_bytes())
            }
            Action::Append(append) => {
                files::path(&append.path, false)?;
                let expected = append.bytes()?;
                (append.path, Some(append.offset), expected)
            }
            _ => {
                return Err(err(
                    "InternalError",
                    "unsupported file reconciliation probe",
                ));
            }
        };
        Ok(Self {
            config: cfg.clone(),
            volume,
            operation_ref: operation.reference(),
            path,
            offset,
            expected,
        })
    }

    pub(crate) fn read(self) -> Result<Sample> {
        let (parent, leaf) = parent(&self.config, &self.volume, &self.path).map_err(unverified)?;
        let parent_revision = file_integrity::Revision::from(&parent.metadata().map_err(unknown)?);
        let entry = child(&parent, &leaf, OFlags::PATH).map_err(unverified)?;
        let meta = entry.metadata().map_err(unknown)?;
        regular(&meta).map_err(unverified)?;
        let begin = self.offset.unwrap_or(0);
        let end = begin + self.expected.len() as u64;
        require(
            meta.len() == end,
            "OutcomeUnknown",
            "file length does not confirm the approved postcondition",
        )?;
        let revision = file_integrity::Revision::from(&meta);
        let file = file_integrity::open_pinned_entry(&entry, &revision).map_err(unverified)?;
        let mut observed = vec![0; self.expected.len()];
        file.read_exact_at(&mut observed, begin).map_err(unknown)?;
        require(
            observed == self.expected,
            "OutcomeUnknown",
            "file bytes do not confirm the approved postcondition",
        )?;

        // A process may have died after append or rename and before these
        // syncs. The database receipt must not get ahead of either boundary.
        file.sync_all().map_err(unknown)?;
        parent.sync_all().map_err(unknown)?;
        let digest = sha256(&observed);
        let receipt = match self.offset {
            Some(offset) => json!({"path":self.path,"offset":offset,"next_offset":end,
                                  "digest":digest,"verified_at":now()}),
            None => json!({"name":self.path,"digest":digest,"verified_at":now()}),
        };
        Ok(Sample {
            operation_ref: self.operation_ref,
            volume_ref: self.volume.reference(),
            path: self.path,
            parent,
            parent_revision,
            file,
            revision,
            receipt,
        })
    }
}

pub(crate) struct Sample {
    operation_ref: Value,
    volume_ref: Value,
    path: String,
    parent: File,
    parent_revision: file_integrity::Revision,
    file: File,
    revision: file_integrity::Revision,
    receipt: Value,
}

impl Sample {
    pub(crate) fn into_receipt(
        self,
        cfg: &Config,
        volume: &Doc,
        operation: &Doc,
    ) -> Result<(Value, Value)> {
        require(
            self.operation_ref == operation.reference() && self.volume_ref == volume.reference(),
            "RecoveryRequired",
            "file observation belongs to another operation or volume revision",
        )?;
        let (current_parent, leaf) = parent(cfg, volume, &self.path).map_err(unverified)?;
        require(
            self.parent_revision
                == file_integrity::Revision::from(&self.parent.metadata().map_err(unknown)?)
                && self.parent_revision
                    == file_integrity::Revision::from(&current_parent.metadata().map_err(unknown)?)
                && self.revision
                    == file_integrity::Revision::from(&self.file.metadata().map_err(unknown)?)
                && self.revision
                    == file_integrity::revision(&current_parent, &leaf).map_err(unverified)?,
            "OutcomeUnknown",
            "the synced file or its parent changed before settlement",
        )?;
        Ok((self.receipt, json!(self.revision)))
    }
}
