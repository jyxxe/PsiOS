//! Authorized, revision-pinned reads of the native backup directory format.
use super::{
    BackupManifest, MAX_BACKUP, MAX_ENTRIES, MAX_MANIFEST, child, copy_hash, detached,
    inspect_detached_backup, open_backup, open_manifest, regular, same_source, staging,
};
use crate::{
    Config, Result,
    db::{Db, Doc},
    decode, err, policy, require,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::{BackupChunk, BackupMember, Client, MAX_ARTIFACT_CHUNK, ResourceRef};
use rustix::fs::OFlags;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{BufReader, Read, Seek, SeekFrom, sink},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex, TryLockError},
};

pub(crate) const ACTION: &str = "backup.export";
pub(crate) const METHOD: &str = "Backup.Export";

#[derive(Default)]
pub(crate) struct Cache(Option<Index>);

struct Index {
    reference: ResourceRef,
    directory: Metadata,
    manifest: Metadata,
    digest: String,
    entries: Vec<Entry>,
}

// Project only the fields used for export. The manifest can include long paths.
#[derive(Deserialize)]
struct Manifest {
    backup_id: String,
    source: Value,
    entries: Vec<Entry>,
    total_bytes: String,
}

#[derive(Deserialize)]
struct Entry {
    bytes: u64,
    digest: Option<String>,
    #[serde(skip)]
    verified: Option<Metadata>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadMember {
    reference: ResourceRef,
    member: BackupMember,
    offset: u64,
    length: usize,
}

pub(crate) struct Job {
    config: Config,
    backup: Doc,
    request: ReadMember,
    cache: Arc<Mutex<Cache>>,
}

fn index(
    backup: &Doc,
    reference: &ResourceRef,
    directory: &Metadata,
    manifest: &mut File,
    metadata: &Metadata,
) -> Result<Index> {
    let (bytes, digest) = copy_hash(manifest, &mut sink(), MAX_MANIFEST)?;
    require(
        bytes == metadata.len() && backup.body["manifest_digest"] == digest,
        "PreconditionFailed",
        "backup manifest integrity failed",
    )?;
    manifest.seek(SeekFrom::Start(0))?;
    let parsed: Manifest =
        serde_json::from_reader(BufReader::new((&mut *manifest).take(MAX_MANIFEST + 1)))?;
    let total = parsed.entries.iter().try_fold(0u64, |sum, entry| {
        sum.checked_add(entry.bytes)
            .filter(|sum| *sum <= MAX_BACKUP)
    });
    require(
        same_source(metadata, &manifest.metadata()?)
            && parsed.backup_id == backup.id
            && parsed.source == backup.spec["source_ref"]
            && backup.body["entries"] == parsed.entries.len()
            && parsed.entries.len() <= MAX_ENTRIES
            && backup.body["bytes"] == parsed.total_bytes
            && total.is_some_and(|sum| parsed.total_bytes.parse() == Ok(sum)),
        "PreconditionFailed",
        "backup manifest disagrees with authority",
    )?;
    Ok(Index {
        reference: reference.clone(),
        directory: directory.clone(),
        manifest: metadata.clone(),
        digest,
        entries: parsed.entries,
    })
}

fn chunk(
    file: &File,
    metadata: &Metadata,
    digest: &str,
    offset: u64,
    length: usize,
) -> Result<BackupChunk> {
    let total_bytes = metadata.len();
    require(
        offset <= total_bytes,
        "InvalidArgument",
        "offset beyond backup member",
    )?;
    let max = (total_bytes - offset).min(length as u64) as usize;
    let mut bytes = vec![0; max];
    file.read_exact_at(&mut bytes, offset)?;
    require(
        same_source(metadata, &file.metadata()?),
        "Conflict",
        "backup member changed during export",
    )?;
    let end = offset + max as u64;
    Ok(BackupChunk {
        offset,
        total_bytes,
        digest: digest.into(),
        next_offset: (end < total_bytes).then_some(end),
        data_base64: STANDARD.encode(bytes),
    })
}

impl Entry {
    fn read(
        &mut self,
        root: &File,
        index: usize,
        offset: u64,
        length: usize,
    ) -> Result<BackupChunk> {
        let Some(digest) = &self.digest else {
            return Err(err("NotFound", "backup member is a directory"));
        };
        let mut file = child(root, format!("blob-{index}"), OFlags::RDONLY)?;
        let metadata = file.metadata()?;
        regular(&metadata)?;
        require(
            metadata.len() == self.bytes,
            "PreconditionFailed",
            "backup member length mismatch",
        )?;
        require(
            offset <= self.bytes,
            "InvalidArgument",
            "offset beyond backup member",
        )?;
        if self
            .verified
            .as_ref()
            .is_none_or(|previous| !same_source(previous, &metadata))
        {
            let (bytes, actual) = copy_hash(&mut file, &mut sink(), self.bytes)?;
            require(
                bytes == self.bytes
                    && actual == *digest
                    && same_source(&metadata, &file.metadata()?),
                "PreconditionFailed",
                "backup member integrity failed",
            )?;
            self.verified = Some(metadata.clone());
        }
        chunk(&file, &metadata, digest, offset, length)
    }
}

fn authorize(db: &Db<'_>, actor: &str, request: &ReadMember) -> Result<Doc> {
    let id = &request.reference.resource_id;
    policy::can_target(db, actor, ACTION, id)?;
    require(
        (1..=MAX_ARTIFACT_CHUNK).contains(&request.length),
        "InvalidArgument",
        "backup export requires a chunk length of 1 to 131072 bytes",
    )?;
    let backup = db.of(id, "Backup")?;
    require(
        backup.body["lifecycle"] == "Stored" && backup.reference() == json!(request.reference),
        "PreconditionFailed",
        "backup reference is no longer available for export",
    )?;
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1)",
        [id],
        |row| row.get(0),
    )?;
    require(!held, "Conflict", "backup is held by another operation")?;
    db.cfg.check_admission_space()?;
    Ok(backup)
}

pub(crate) fn prepare(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
    cache: Arc<Mutex<Cache>>,
) -> Result<Job> {
    let request = decode(params)?;
    let backup = authorize(db, actor, &request)?;
    Ok(Job {
        config: db.cfg.clone(),
        backup,
        request,
        cache,
    })
}

pub(crate) fn finish(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
    result: Result<BackupChunk>,
) -> Result<Value> {
    // Revocation and deletion can commit while the file is being hashed.
    let backup = authorize(db, actor, &decode(params)?)?;
    let reply = result?;
    db.audit(actor, METHOD, Some(&backup.id), "Allowed")?;
    Ok(serde_json::to_value(reply)?)
}

impl Job {
    pub(crate) fn read(self) -> Result<BackupChunk> {
        let mut cache = self.cache.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => {
                let mut error = err("QuotaExceeded", "backup export reader is busy");
                error.0.retryable = true;
                error.0.retry_condition = "SameRequestAfterBackoff".into();
                error
            }
            TryLockError::Poisoned(_) => {
                err("RecoveryRequired", "backup export reader was interrupted")
            }
        })?;
        let request = self.request;
        let (_, root) = open_backup(&self.config, &self.backup.id)?;
        let directory = root.metadata()?;
        let mut manifest = open_manifest(&root)?;
        let metadata = manifest.metadata()?;
        if cache.0.as_ref().is_none_or(|index| {
            index.reference != request.reference
                || !same_source(&index.directory, &directory)
                || !same_source(&index.manifest, &metadata)
        }) {
            cache.0 = None;
            cache.0 = Some(index(
                &self.backup,
                &request.reference,
                &directory,
                &mut manifest,
                &metadata,
            )?);
        }
        let index = cache.0.as_mut().unwrap();
        let reply = match request.member {
            BackupMember::Manifest => chunk(
                &manifest,
                &metadata,
                &index.digest,
                request.offset,
                request.length,
            )?,
            BackupMember::Blob { index: member } => index
                .entries
                .get_mut(member)
                .ok_or_else(|| err("NotFound", "backup member unavailable"))?
                .read(&root, member, request.offset, request.length)?,
        };
        require(
            same_source(&directory, &root.metadata()?)
                && same_source(&metadata, &manifest.metadata()?),
            "Conflict",
            "backup directory or manifest changed during export",
        )?;
        Ok(reply)
    }
}

/// Export an authority-pinned backup to a new directory beneath a private parent.
pub fn download_backup(client: &Client, backup: &ResourceRef, output: &Path) -> Result<Value> {
    let (parent, name) = detached::output_parent(None, output)?;
    let stage = staging::create(&parent)?;
    let create_file = |name: &str| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage.path().join(name))
    };
    let mut file = create_file("manifest.json")?;
    let receipt =
        client.download_backup_member(backup, BackupMember::Manifest, MAX_MANIFEST, &mut file)?;
    file.sync_all()?;
    drop(file);
    let manifest: BackupManifest = serde_json::from_reader(BufReader::new(File::open(
        stage.path().join("manifest.json"),
    )?))?;
    require(
        manifest.backup_id == backup.resource_id && manifest.entries.len() <= MAX_ENTRIES,
        "PreconditionFailed",
        "export manifest identity or entry limit mismatch",
    )?;
    let files = manifest
        .entries
        .iter()
        .filter(|entry| entry.digest.is_some());
    let lengths = files.clone().map(|entry| entry.bytes);
    require(
        lengths
            .clone()
            .try_fold(0u64, u64::checked_add)
            .is_some_and(|total| total <= MAX_BACKUP),
        "QuotaExceeded",
        "backup export exceeds 256 MiB",
    )?;
    crate::db::check_restore_capacity(stage.directory(), lengths, files.count() as u64, 0)?;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if let Some(expected) = &entry.digest {
            let mut file = create_file(&format!("blob-{index}"))?;
            let member = client.download_backup_member(
                backup,
                BackupMember::Blob { index },
                entry.bytes,
                &mut file,
            )?;
            require(
                member.bytes == entry.bytes && member.digest == *expected,
                "PreconditionFailed",
                "exported member disagrees with backup manifest",
            )?;
            file.sync_all()?;
        }
    }
    let mut report = inspect_detached_backup(stage.path(), &receipt.digest, None)?;
    stage.publish(name)?;
    report["backup_ref"] = json!(backup);
    report["exported_directory"] = json!(parent.join(name).display().to_string());
    Ok(report)
}
