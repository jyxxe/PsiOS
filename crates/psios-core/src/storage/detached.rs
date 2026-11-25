//! Verify or extract a copied backup with an independently obtained manifest digest.
use super::{
    BackupManifest, RestoreDestination, filename, inspect_members, open_manifest, publication,
    read_manifest, staging,
};
use crate::{DEFAULT_BACKUP_MAX_BYTES, MAX_BACKUP_BYTES, Result, err, require};
use psios_api::ResourceRef;
use rustix::fs::{Mode, OFlags, open};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::Path,
};

pub(super) fn expected_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(super) fn output_parent<'a>(
    source: Option<&File>,
    output: &'a Path,
    require_new: bool,
) -> Result<(std::path::PathBuf, &'a str)> {
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| err("InvalidArgument", "a new output directory name is required"))?;
    filename(name)?;
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    if let Some(source) = source {
        let (source, target) = (source.metadata()?, parent.metadata()?);
        require(
            source.dev() != target.dev() || source.ino() != target.ino(),
            "InvalidArgument",
            "restore output must be outside the backup directory",
        )?;
    }
    match fs::symlink_metadata(parent.join(name)) {
        Ok(_) if require_new => return Err(err("Conflict", "output already exists")),
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok((parent, name))
}

pub(super) struct DetachedBackup {
    pub root: File,
    pub manifest: BackupManifest,
    pub source: ResourceRef,
    before: (String, usize),
}

impl DetachedBackup {
    pub fn confirm(&self) -> Result<()> {
        require(
            self.before == publication::file_set(&self.root)?,
            "PreconditionFailed",
            "backup file set changed during inspection",
        )
    }
}

pub(super) fn open_detached(
    directory: &Path,
    manifest_digest: &str,
) -> Result<(DetachedBackup, Vec<u8>)> {
    require(
        expected_digest(manifest_digest),
        "InvalidArgument",
        "expected a SHA-256 manifest digest",
    )?;
    let root = File::from(
        open(
            directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    let digest = json!(manifest_digest);
    let before = publication::file_set(&root)?;
    let raw = read_manifest(&open_manifest(&root)?, &digest)?;
    let manifest: BackupManifest = serde_json::from_slice(&raw)?;
    filename(&manifest.backup_id)?;
    let source: ResourceRef = serde_json::from_value(manifest.source.clone())?;
    require(
        before.1
            == 1 + manifest
                .entries
                .iter()
                .filter(|entry| entry.digest.is_some())
                .count(),
        "PreconditionFailed",
        "backup file set does not match the manifest",
    )?;
    Ok((
        DetachedBackup {
            root,
            manifest,
            source,
            before,
        },
        raw,
    ))
}

/// Inspect an ordinary local copy of a PsiOS backup, using a trusted external manifest digest.
/// An optional new directory is atomically published with the verified file tree.
pub fn inspect_detached_backup(
    directory: &Path,
    manifest_digest: &str,
    output: Option<&Path>,
) -> Result<Value> {
    inspect_detached_backup_with_limit(directory, manifest_digest, output, DEFAULT_BACKUP_MAX_BYTES)
}

pub fn inspect_detached_backup_with_limit(
    directory: &Path,
    manifest_digest: &str,
    output: Option<&Path>,
    max_bytes: u64,
) -> Result<Value> {
    require(
        (1..=MAX_BACKUP_BYTES).contains(&max_bytes),
        "InvalidArgument",
        "invalid detached backup byte limit",
    )?;
    let (archive, _) = open_detached(directory, manifest_digest)?;
    let destination = output
        .map(|path| output_parent(Some(&archive.root), path, true))
        .transpose()?;
    let stage = destination
        .as_ref()
        .map(|(parent, _)| staging::create(parent))
        .transpose()?;
    let mut report = inspect_members(
        &archive.root,
        &archive.manifest,
        &json!(manifest_digest),
        stage.as_ref().map(|stage| RestoreDestination {
            directory: stage.directory(),
            authority: None,
        }),
        max_bytes,
    )?;
    archive.confirm()?;
    if let Some(((parent, name), stage)) = destination.zip(stage) {
        stage.publish(name)?;
        report["restored_directory"] = json!(parent.join(name).display().to_string());
    }
    report["backup_id"] = json!(archive.manifest.backup_id);
    report["source_ref"] = json!(archive.source);
    Ok(report)
}
