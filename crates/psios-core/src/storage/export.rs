//! Authorized, revision-pinned reads of the native backup directory format.
mod integrity;

use super::{
    BackupManifest, MAX_ENTRIES, MAX_MANIFEST, child, copy_hash, detached, file_sizes,
    inspect_detached_backup_with_limit, open_backup, open_manifest, regular, same_source, staging,
    validate_members,
};
use crate::{
    Config, DEFAULT_BACKUP_MAX_BYTES, MAX_BACKUP_BYTES, Result,
    config::private_dir,
    db::{Db, Doc},
    decode, err, policy, require,
};
use base64::{Engine, engine::general_purpose::STANDARD};
pub(crate) use integrity::Cache;
use integrity::MemberIntegrity;
use psios_api::{
    BackupChunk, BackupMember, BackupMemberReceipt, Client, MAX_ARTIFACT_CHUNK, ResourceRef,
    canonical, sha256,
};
use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, Metadata},
    io::{BufReader, ErrorKind, Read, Seek, SeekFrom, sink},
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    sync::Arc,
};

pub(crate) const ACTION: &str = "backup.export";
pub(crate) const METHOD: &str = "Backup.Export";
const RESUME_PREFIX: &str = ".psios-backup-export-";

struct Index {
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
    integrity: MemberIntegrity,
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
    cache: Arc<Cache>,
}

fn index(
    backup: &Doc,
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
            .filter(|sum| *sum <= MAX_BACKUP_BYTES)
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
        let file = child(root, format!("blob-{index}"), OFlags::RDONLY)?;
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
        self.integrity
            .verify(&file, &metadata, self.bytes, digest)?;
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

pub(crate) fn prepare(db: &Db<'_>, actor: &str, params: &Value, cache: Arc<Cache>) -> Result<Job> {
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
        let request = self.request;
        let slot = self.cache.checkout(&request.reference)?;
        let mut cached = slot.try_lock().map_err(integrity::lock_error)?;
        let (_, root) = open_backup(&self.config, &self.backup.id)?;
        let directory = root.metadata()?;
        let mut manifest = open_manifest(&root)?;
        let metadata = manifest.metadata()?;
        if cached.as_ref().is_none_or(|index| {
            !same_source(&index.directory, &directory) || !same_source(&index.manifest, &metadata)
        }) {
            *cached = None;
            *cached = Some(index(&self.backup, &directory, &mut manifest, &metadata)?);
        }
        let index = cached.as_mut().unwrap();
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
    download_backup_with_limit(client, backup, output, DEFAULT_BACKUP_MAX_BYTES)
}

pub fn download_backup_with_limit(
    client: &Client,
    backup: &ResourceRef,
    output: &Path,
    max_bytes: u64,
) -> Result<Value> {
    export_backup(client, backup, output, max_bytes, false)
}

/// Continue the private export associated with this pinned backup and output name.
pub fn resume_backup_with_limit(
    client: &Client,
    backup: &ResourceRef,
    output: &Path,
    max_bytes: u64,
) -> Result<Value> {
    export_backup(client, backup, output, max_bytes, true)
}

fn staged_member(stage: &staging::Stage, name: &str, resumable: bool) -> Result<File> {
    let access = if resumable {
        OFlags::RDWR
    } else {
        OFlags::WRONLY | OFlags::EXCL
    };
    let file = File::from(
        openat2(
            stage.directory(),
            name,
            access | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(std::io::Error::from)?,
    );
    private_member(stage, &file.metadata()?)?;
    Ok(file)
}

fn private_member(stage: &staging::Stage, meta: &Metadata) -> Result<()> {
    regular(meta)?;
    require(
        meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode() & 0o077 == 0
            && meta.dev() == stage.directory().metadata()?.dev(),
        "PermissionDenied",
        "backup staging member is outside the private export",
    )
}

fn check_export_space(
    stage: &staging::Stage,
    manifest: &BackupManifest,
    resumable: bool,
) -> Result<()> {
    if !resumable {
        let sizes = file_sizes(manifest);
        return crate::db::check_restore_capacity(
            stage.directory(),
            sizes.clone(),
            sizes.count() as u64,
            0,
        );
    }
    let mut new_entries = 0;
    let mut remaining = Vec::new();
    for (index, entry) in manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.digest.is_some())
    {
        let path = stage.path().join(format!("blob-{index}"));
        let written = match fs::symlink_metadata(path) {
            Ok(meta) => {
                private_member(stage, &meta)?;
                meta.blocks().saturating_mul(512).min(meta.len())
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                new_entries += 1;
                0
            }
            Err(e) => return Err(e.into()),
        };
        remaining.push(entry.bytes.saturating_sub(written));
    }
    crate::db::check_restore_capacity(stage.directory(), remaining.into_iter(), new_entries, 0)
}

fn transfer_member(
    client: &Client,
    backup: &ResourceRef,
    member: BackupMember,
    max_bytes: u64,
    file: &mut File,
    resumable: bool,
) -> Result<BackupMemberReceipt> {
    if resumable {
        let had_prefix = file.metadata()?.len() > 0;
        match client.resume_backup_member(backup, member, max_bytes, file) {
            Ok(receipt) => return Ok(receipt),
            Err(e)
                if had_prefix
                    && matches!(e.code.as_str(), "RecoveryRequired" | "InvalidArgument") =>
            {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(client.download_backup_member(backup, member, max_bytes, file)?)
}

fn export_backup(
    client: &Client,
    backup: &ResourceRef,
    output: &Path,
    max_bytes: u64,
    resumable: bool,
) -> Result<Value> {
    require(
        (1..=MAX_BACKUP_BYTES).contains(&max_bytes),
        "InvalidArgument",
        "invalid backup export byte limit",
    )?;
    let (parent, name) = detached::output_parent(None, output, !resumable)?;
    require(
        !name.starts_with(RESUME_PREFIX),
        "InvalidArgument",
        "output name is reserved for staged backup exports",
    )?;
    let published = parent.join(name);
    let stage = if resumable {
        private_dir(&parent, false)?;
        match fs::symlink_metadata(&published) {
            Ok(_) => {
                let receipt = client.download_backup_member(
                    backup,
                    BackupMember::Manifest,
                    MAX_MANIFEST,
                    &mut sink(),
                )?;
                let mut report = inspect_detached_backup_with_limit(
                    &published,
                    &receipt.digest,
                    None,
                    max_bytes,
                )?;
                require(
                    report["backup_id"] == backup.resource_id,
                    "PreconditionFailed",
                    "output belongs to a different backup",
                )?;
                report["backup_ref"] = json!(backup);
                report["exported_directory"] = json!(published.display().to_string());
                return Ok(report);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let identity = sha256(&canonical(&json!(["psios-backup-export-v1", name, backup])));
        staging::resume(&parent, &format!("{RESUME_PREFIX}{}", &identity[7..]))?
    } else {
        staging::create(&parent)?
    };
    let mut file = staged_member(&stage, "manifest.json", resumable)?;
    let receipt = transfer_member(
        client,
        backup,
        BackupMember::Manifest,
        MAX_MANIFEST,
        &mut file,
        resumable,
    )?;
    file.sync_all()?;
    drop(file);
    let manifest: BackupManifest =
        serde_json::from_reader(BufReader::new(open_manifest(stage.directory())?))?;
    require(
        manifest.backup_id == backup.resource_id,
        "PreconditionFailed",
        "export manifest identity disagrees with the pinned backup",
    )?;
    validate_members(&manifest, max_bytes)?;
    check_export_space(&stage, &manifest, resumable)?;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if let Some(expected) = &entry.digest {
            let mut file = staged_member(&stage, &format!("blob-{index}"), resumable)?;
            let member = transfer_member(
                client,
                backup,
                BackupMember::Blob { index },
                entry.bytes,
                &mut file,
                resumable,
            )?;
            require(
                member.bytes == entry.bytes && member.digest == *expected,
                "PreconditionFailed",
                "exported member disagrees with backup manifest",
            )?;
            file.sync_all()?;
        }
    }
    let mut report =
        inspect_detached_backup_with_limit(stage.path(), &receipt.digest, None, max_bytes)?;
    stage.publish(name)?;
    report["backup_ref"] = json!(backup);
    report["exported_directory"] = json!(published.display().to_string());
    Ok(report)
}
