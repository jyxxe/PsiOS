use super::{
    backup_blob, copy_hash,
    detached::{DetachedBackup, expected_digest, open_detached},
    file_sizes, inspect_members, open_backup, staging,
    transfer::{Attestation, ReceiverAdmission},
    validate_members,
};
use crate::{
    Config, MAX_BACKUP_BYTES, Result, Store,
    config::new_file,
    db::{Db, Doc},
    decode, err, label, policy, require,
};
use psios_api::{ResourceRef, VERSION};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

pub(super) const KIND: &str = "BackupIntake";
const METHOD: &str = "Backup.ImportDetached";

#[derive(Deserialize)]
struct Origin {
    schema_version: String,
    scope: String,
    consistency: String,
    data_incarnation: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intake {
    pub(super) backup_id: String,
    pub(super) source: ResourceRef,
    pub(super) data_incarnation: String,
    pub(super) manifest_digest: String,
    pub(super) entries: usize,
    pub(super) bytes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) transfer: Option<Attestation>,
}

impl Intake {
    pub(super) fn from_archive(archive: &DetachedBackup, raw: &[u8], digest: &str) -> Result<Self> {
        let origin: Origin = serde_json::from_slice(raw)?;
        let manifest = &archive.manifest;
        require(
            origin.schema_version == VERSION
                && origin.scope == "single_volume"
                && origin.consistency == "crash_consistent",
            "UnsupportedCapability",
            "import requires a native crash-consistent single-volume backup",
        )?;
        require(
            manifest
                .backup_id
                .strip_prefix("backup-op-")
                .is_some_and(|suffix| {
                    uuid::Uuid::parse_str(suffix).is_ok_and(|id| id.to_string() == suffix)
                }),
            "InvalidArgument",
            "detached backup does not have a native backup identity",
        )?;
        label(&archive.source.resource_id)?;
        label(&archive.source.version)?;
        require(
            expected_digest(&archive.source.digest),
            "InvalidArgument",
            "invalid backup source reference digest",
        )?;
        label(&origin.data_incarnation)?;
        let declared = validate_members(manifest, MAX_BACKUP_BYTES)?;
        require(
            manifest.total_bytes == declared.to_string(),
            "PreconditionFailed",
            "invalid backup length encoding",
        )?;
        Ok(Self {
            backup_id: manifest.backup_id.clone(),
            source: archive.source.clone(),
            data_incarnation: origin.data_incarnation,
            manifest_digest: digest.into(),
            entries: manifest.entries.len(),
            bytes: declared.to_string(),
            transfer: None,
        })
    }

    fn compatible_backup(&self, db: &Db<'_>) -> Result<Option<Doc>> {
        let backup = db.find(&self.backup_id)?;
        if let Some(backup) = &backup {
            require(
                backup.kind == "Backup"
                    && backup.body["lifecycle"] == "Stored"
                    && backup.spec["source_ref"] == json!(self.source)
                    && backup.body["manifest_digest"] == self.manifest_digest
                    && backup.body["entries"] == self.entries
                    && backup.body["bytes"] == self.bytes,
                "Conflict",
                "backup identity is already bound to another resource or manifest",
            )?;
        }
        Ok(backup)
    }

    fn verify_published(&self, path: &Path, expected: Option<&Value>) -> Result<()> {
        let (archive, _) = open_detached(path, &self.manifest_digest)?;
        if let Some(expected) = expected {
            require(
                directory_identity(&archive.root)? == *expected,
                "RecoveryRequired",
                "published directory differs from the witnessed intake",
            )?;
        }
        require(
            archive.manifest.backup_id == self.backup_id
                && archive.source == self.source
                && archive.manifest.entries.len() == self.entries
                && archive.manifest.total_bytes == self.bytes,
            "RecoveryRequired",
            "published backup differs from the witnessed intake",
        )?;
        self.verify_archive(&archive)
    }

    pub(super) fn verify_archive(&self, archive: &DetachedBackup) -> Result<()> {
        inspect_members(
            &archive.root,
            &archive.manifest,
            &json!(self.manifest_digest),
            None,
            MAX_BACKUP_BYTES,
        )?;
        archive.confirm()
    }
}

fn directory_identity(root: &File) -> Result<Value> {
    let stat = root.metadata()?;
    Ok(json!([stat.dev().to_string(), stat.ino().to_string()]))
}

fn copy_archive(
    store: &Store,
    archive: &DetachedBackup,
    raw: &[u8],
    stage: &staging::Stage,
) -> Result<()> {
    let files = file_sizes(&archive.manifest);
    store.config.check_managed_space(
        stage.directory(),
        files.clone().chain(std::iter::once(raw.len() as u64)),
        files.count() as u64 + 1,
    )?;
    for (index, entry) in archive.manifest.entries.iter().enumerate() {
        let Some(expected) = &entry.digest else {
            continue;
        };
        let mut input = backup_blob(&archive.root, index, entry.bytes)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(stage.path().join(format!("blob-{index}")))?;
        let (bytes, digest) = copy_hash(&mut input, &mut output, entry.bytes)?;
        require(
            bytes == entry.bytes && digest == *expected,
            "PreconditionFailed",
            "backup content integrity failed",
        )?;
        output.sync_all()?;
    }
    archive.confirm()?;
    new_file(&stage.path().join("manifest.json"), raw)?;
    store
        .config
        .check_managed_space(stage.directory(), std::iter::empty(), 0)
}

fn receipt(backup: &Doc, reused: bool) -> Value {
    json!({"reference":backup.reference(), "manifest_digest":backup.body["manifest_digest"], "reused":reused})
}

fn settle(db: &Db<'_>, intake: &Doc, info: &Intake, reused: bool) -> Result<Value> {
    let (_, root) = open_backup(db.cfg, &info.backup_id)?;
    require(
        directory_identity(&root)? == intake.body["directory"],
        "RecoveryRequired",
        "published directory differs from the witnessed intake",
    )?;
    let existing = info.compatible_backup(db)?;
    if intake.body["lifecycle"] == "Settled" {
        let backup = existing
            .filter(|backup| backup.reference() == intake.body["backup_ref"])
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "settled backup intake has no matching resource",
                )
            })?;
        return Ok(receipt(&backup, true));
    }
    require(
        existing.is_none(),
        "Conflict",
        "backup identity was registered by another operation",
    )?;
    let backup = db.create(
        "Backup",
        &info.backup_id,
        policy::OWNER,
        json!({"source_ref":info.source, "source_data_incarnation":info.data_incarnation,
               "intake_ref":intake.reference()}),
        json!({"lifecycle":"Stored", "manifest_digest":info.manifest_digest,
               "entries":info.entries, "bytes":info.bytes, "consistency":"crash_consistent",
               "failure_domain":"local-filesystem", "coordination":"verified detached copy"}),
    )?;
    let mut state = intake.body.clone();
    state["lifecycle"] = json!("Settled");
    state["backup_ref"] = backup.reference();
    db.status(intake, state)?;
    db.audit(policy::OWNER, METHOD, Some(&backup.id), "Imported")?;
    Ok(receipt(&backup, reused))
}

pub(super) fn intake_id(digest: &str) -> String {
    format!("backup-intake-{}", &digest[7..])
}

impl Store {
    /// Register a verified copy under the stopped authority's local storage catalog.
    pub fn import_detached_backup(config: Config, directory: &Path, digest: &str) -> Result<Value> {
        Self::import_backup(config, directory, digest, None)
    }

    /// Register a detached backup accompanied by a recipient-bound source attestation.
    pub fn import_signed_detached_backup(
        config: Config,
        directory: &Path,
        digest: &str,
        attestation: &Value,
        trusted_key: &Path,
        expected_authority: &str,
    ) -> Result<Value> {
        let admission =
            ReceiverAdmission::read(attestation, trusted_key, expected_authority, digest)?;
        Self::import_backup(config, directory, digest, Some(admission))
    }

    fn import_backup(
        config: Config,
        directory: &Path,
        digest: &str,
        admission: Option<ReceiverAdmission>,
    ) -> Result<Value> {
        require(
            expected_digest(digest),
            "InvalidArgument",
            "expected a SHA-256 manifest digest",
        )?;
        let id = intake_id(digest);
        let mut store = Self::open(config)?;
        let parent = store.config.state_dir.join("backups");
        let existing = store.transaction(&id, |db| {
            if let Some(admission) = &admission {
                admission.check_recipient(&db.meta("authority")?)?;
            }
            let Some(intake) = db.find(&id)? else {
                return Ok(None);
            };
            let info: Intake = decode(&intake.spec)?;
            require(
                intake.kind == KIND
                    && intake.owner == policy::OWNER
                    && info.manifest_digest == digest
                    && json!(info) == intake.spec
                    && matches!(
                        intake.body["lifecycle"].as_str(),
                        Some("Prepared" | "Settled")
                    )
                    && info
                        .transfer
                        .as_ref()
                        .is_none_or(|envelope| envelope.matches(&info)),
                "RecoveryRequired",
                "invalid witnessed backup intake",
            )?;
            ReceiverAdmission::verify_existing(db.c, &id, &info, admission.as_ref())?;
            Ok(Some((intake, info)))
        })?;
        if let Some((intake, info)) = &existing {
            super::filename(&info.backup_id)?;
            let published = parent.join(&info.backup_id);
            match fs::symlink_metadata(&published) {
                Ok(_) => {
                    info.verify_published(&published, Some(&intake.body["directory"]))?;
                    return store.transaction(&id, |db| settle(db, intake, info, true));
                }
                Err(e)
                    if e.kind() == ErrorKind::NotFound
                        && intake.body["lifecycle"] == "Prepared" => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(err(
                        "RecoveryRequired",
                        "settled backup intake directory is missing",
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }

        let (archive, raw) = open_detached(directory, digest)?;
        let mut info = Intake::from_archive(&archive, &raw, digest)?;
        if let Some(admission) = &admission {
            require(
                admission.envelope.matches(&info),
                "PreconditionFailed",
                "backup manifest provenance differs from its transfer attestation",
            )?;
            info.transfer = Some(admission.envelope.clone());
        }
        let spec = json!(info);
        if let Some((intake, _)) = &existing {
            require(
                intake.spec == spec,
                "RecoveryRequired",
                "backup intake disagrees with the source",
            )?;
        }
        let path = parent.join(&info.backup_id);
        let registered = store.transaction(&id, |db| {
            let competing: bool = db.c.query_row(
                "SELECT EXISTS(SELECT 1 FROM current WHERE kind=?1 AND id<>?2 AND spec->>'backup_id'=?3)",
                params![KIND, id, info.backup_id], |row| row.get(0),
            )?;
            require(!competing, "Conflict", "backup identity is already reserved by another intake")?;
            info.compatible_backup(db)
        })?;
        if let Some(backup) = registered {
            require(
                admission.is_none(),
                "Conflict",
                "an existing Backup cannot acquire signed transfer provenance",
            )?;
            info.verify_published(&path, None)?;
            return Ok(receipt(&backup, true));
        }
        require(
            info.bytes
                .parse::<u64>()
                .is_ok_and(|bytes| bytes <= store.config.backup_max_bytes),
            "QuotaExceeded",
            "backup exceeds the local import byte limit",
        )?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(err(
                    "Conflict",
                    "unregistered backup directory requires separate reconciliation",
                ));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let stage = staging::create(&parent, |root| {
            store
                .config
                .check_managed_space(root, std::iter::empty(), 1)
        })?;
        copy_archive(&store, &archive, &raw, &stage)?;
        let state =
            json!({"lifecycle":"Prepared", "directory":directory_identity(stage.directory())?});
        let intake = store.transaction(&id, |db| {
            let current = match &existing {
                Some((intake, _)) => db.status(intake, state)?,
                None => {
                    let prepared = db.create(KIND, &id, policy::OWNER, spec, state)?;
                    if let Some(admission) = &admission {
                        admission.persist(db.c, &prepared.id)?;
                    }
                    prepared
                }
            };
            db.audit(policy::OWNER, METHOD, Some(&info.backup_id), "Prepared")?;
            Ok(current)
        })?;
        stage.publish(&info.backup_id)?;
        store.transaction(&id, |db| settle(db, &intake, &info, false))
    }
}
