//! Offline observation and catalog settlement of one witnessed detached Backup.
use super::{Intake, directory_identity, intake_id, settle, witnessed};
use crate::storage::{
    child,
    detached::{DetachedBackup, expected_digest, open_detached_root},
    open_backup_parent, transfer,
};
use crate::{Config, MAX_BACKUP_BYTES, Result, Store, db, decode, err, policy, require};
use psios_api::ResourceRef;
use rusqlite::Connection;
use rustix::{
    fs::{AtFlags, FileType, OFlags, Stat, statat},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::File, io, os::unix::fs::MetadataExt};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    authority_id: String,
    recovery_incarnation: String,
    intake_ref: ResourceRef,
}

pub(super) struct Publication {
    parent: File,
    archive: DetachedBackup,
}

fn final_entry(parent: &File, name: &str) -> Result<Option<Stat>> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(entry) => Ok(Some(entry)),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(io::Error::from(error).into()),
    }
}

fn exact_entry(entry: &Stat, identity: &Value) -> bool {
    FileType::from_raw_mode(entry.st_mode) == FileType::Directory
        && entry.st_uid == rustix::process::geteuid().as_raw()
        && entry.st_mode & 0o077 == 0
        && *identity == json!([entry.st_dev.to_string(), entry.st_ino.to_string()])
}

impl Publication {
    pub(super) fn observe(
        config: &Config,
        intake: &db::Doc,
        info: &Intake,
        max_bytes: u64,
    ) -> Result<Option<Self>> {
        let parent = open_backup_parent(config)?;
        let Some(entry) = final_entry(&parent, &info.backup_id)? else {
            return Ok(None);
        };
        let identity = &intake.body["directory"];
        require(
            exact_entry(&entry, identity),
            "RecoveryRequired",
            "published backup directory differs from the witnessed intake",
        )?;
        let root = child(
            &parent,
            info.backup_id.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
        )?;
        let metadata = root.metadata()?;
        require(
            directory_identity(&root)? == *identity
                && metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o077 == 0,
            "RecoveryRequired",
            "published backup directory differs from the witnessed intake",
        )?;
        let (archive, raw) = open_detached_root(root, &info.manifest_digest)?;
        info.verify_manifest(&archive, &raw)?;
        info.verify_archive_with_limit(&archive, max_bytes)?;
        let publication = Self { parent, archive };
        publication.confirm_entry(intake, info)?;
        Ok(Some(publication))
    }

    fn confirm_entry(&self, intake: &db::Doc, info: &Intake) -> Result<()> {
        require(
            final_entry(&self.parent, &info.backup_id)?
                .is_some_and(|entry| exact_entry(&entry, &intake.body["directory"])),
            "RecoveryRequired",
            "published backup directory changed after verification",
        )
    }

    pub(super) fn settle(&self, db: &db::Db<'_>, intake: &db::Doc, info: &Intake) -> Result<Value> {
        if intake.body["lifecycle"] == "Prepared" {
            // The interrupted writer might have crossed rename but never synced
            // the new name. Do not commit catalog ownership until it is durable.
            self.archive.root.sync_all()?;
            self.parent.sync_all()?;
        }
        self.confirm_entry(intake, info)?;
        self.archive.confirm()?;
        settle(db, intake, info, true)
    }
}

struct Registration {
    backup: db::Doc,
    prepared: db::Doc,
    retired: bool,
}

fn registration(
    conn: &Connection,
    intake: &db::Doc,
    info: &Intake,
) -> Result<Option<Registration>> {
    let corrupt = || {
        err(
            "RecoveryRequired",
            "backup intake has inconsistent catalog custody",
        )
    };
    let current = db::find(conn, &info.backup_id)?;
    if intake.body["lifecycle"] == "Prepared" {
        require(
            current.is_none() && intake.body.get("backup_ref").is_none(),
            "RecoveryRequired",
            "prepared backup intake conflicts with catalog custody",
        )?;
        return Ok(None);
    }

    let backup = db::resolve(conn, &intake.body["backup_ref"]).map_err(|_| corrupt())?;
    let prepared = db::resolve(conn, &backup.spec["intake_ref"]).map_err(|_| corrupt())?;
    let current = current.ok_or_else(corrupt)?;
    let retired = current.body["lifecycle"] == "Deleted";
    require(
        prepared.id == intake.id
            && prepared.rev.checked_add(1) == Some(intake.rev)
            && witnessed(&prepared, &info.manifest_digest)? == *info
            && prepared.body["lifecycle"] == "Prepared"
            && prepared.body["directory"] == intake.body["directory"]
            && backup.kind == "Backup"
            && backup.owner == policy::OWNER
            && backup.id == info.backup_id
            && backup.rev == 1
            && backup.body["lifecycle"] == "Stored"
            && backup.body["manifest_digest"] == info.manifest_digest
            && backup.body["entries"] == info.entries
            && backup.body["bytes"] == info.bytes
            && backup.spec["source_ref"] == json!(info.source)
            && backup.spec["source_data_incarnation"] == info.data_incarnation
            && current.kind == backup.kind
            && current.owner == backup.owner
            && current.spec == backup.spec
            && if retired {
                current.rev == 2 && current.body["manifest_digest"] == info.manifest_digest
            } else {
                current.reference() == backup.reference()
            },
        "RecoveryRequired",
        "backup intake has inconsistent catalog custody",
    )?;
    Ok(Some(Registration {
        backup,
        prepared,
        retired,
    }))
}

pub(crate) fn inspect(
    conn: &Connection,
    config: &Config,
    format: u32,
    authority: &str,
    incarnation: &str,
    digest: &str,
    max_bytes: u64,
) -> Result<Value> {
    require(
        expected_digest(digest),
        "InvalidArgument",
        "expected a SHA-256 manifest digest",
    )?;
    check_limit(max_bytes)?;
    let id = intake_id(digest);
    let intake = db::find(conn, &id)?
        .ok_or_else(|| err("NotFound", "no witnessed intake for this backup manifest"))?;
    let info = witnessed(&intake, digest)?;
    transfer::verify_retained(conn, &id, &info, authority, format >= transfer::FORMAT)?;
    let registered = registration(conn, &intake, &info)?;
    let retired = registered.as_ref().is_some_and(|value| value.retired);
    let placement = if retired {
        let parent = open_backup_parent(config)?;
        if final_entry(&parent, &info.backup_id)?.is_some() {
            "Conflict"
        } else {
            "Retired"
        }
    } else {
        match Publication::observe(config, &intake, &info, max_bytes) {
            Ok(Some(_)) => "VerifiedPublished",
            Ok(None) => "NoPublishedDirectory",
            Err(error)
                if matches!(
                    error.0.code.as_str(),
                    "RecoveryRequired"
                        | "PreconditionFailed"
                        | "UnsupportedCapability"
                        | "InvalidArgument"
                ) =>
            {
                "Conflict"
            }
            Err(error) => return Err(error),
        }
    };
    let mut report = json!({
        "authority_id":authority,
        "recovery_incarnation":incarnation,
        "intake_ref":intake.reference(),
        "lifecycle":intake.body["lifecycle"],
        "manifest_digest":digest,
        "mode":if info.transfer.is_some() { "Signed" } else { "DigestOnly" },
        "publication":placement,
    });
    if let Some(attestation) = &info.transfer {
        report["signer_fingerprint"] = json!(attestation.fingerprint());
    }
    if let Some(registered) = registered {
        report["backup_ref"] = registered.backup.reference();
    } else if placement == "VerifiedPublished" {
        report["recovery"] = json!(Recovery {
            authority_id: authority.to_owned(),
            recovery_incarnation: incarnation.to_owned(),
            intake_ref: decode(&intake.reference())?,
        });
    }
    Ok(report)
}

fn check_limit(max_bytes: u64) -> Result<()> {
    require(
        (1..=MAX_BACKUP_BYTES).contains(&max_bytes),
        "InvalidArgument",
        "invalid backup intake verification byte limit",
    )
}

impl Store {
    /// Settle only the witnessed publication selected by exact stopped-authority inspection.
    pub fn reconcile_backup_intake(
        config: Config,
        approval: &Value,
        max_bytes: u64,
    ) -> Result<Value> {
        check_limit(max_bytes)?;
        let approved: Recovery = decode(approval)?;
        let id = &approved.intake_ref.resource_id;
        let suffix = id.strip_prefix("backup-intake-").ok_or_else(|| {
            err(
                "InvalidArgument",
                "expected an exact backup intake reference",
            )
        })?;
        let digest = format!("sha256:{suffix}");
        require(
            expected_digest(&digest) && expected_digest(&approved.intake_ref.digest),
            "InvalidArgument",
            "expected an exact backup intake reference",
        )?;
        let mut store = Self::open(config)?;
        let (intake, info) = store.transaction(id, |db| {
            let authority = db.meta("authority")?;
            require(
                authority == approved.authority_id
                    && db.incarnation == approved.recovery_incarnation,
                "Conflict",
                "backup intake inspection belongs to a different authority or recovery incarnation",
            )?;
            let intake = db.get(id)?;
            let info = witnessed(&intake, &digest)?;
            transfer::verify_retained(db.c, id, &info, &authority, true)?;
            let registered = registration(db.c, &intake, &info)?;
            let selected = registered.as_ref().map_or(&intake, |value| &value.prepared);
            require(
                selected.reference() == json!(approved.intake_ref),
                "Conflict",
                "backup intake revision differs from the approved inspection",
            )?;
            require(
                !registered.is_some_and(|value| value.retired),
                "PreconditionFailed",
                "imported backup has been retired",
            )?;
            Ok((intake, info))
        })?;
        let published = Publication::observe(&store.config, &intake, &info, max_bytes)?
            .ok_or_else(|| {
                err(
                    "PreconditionFailed",
                    "backup intake has no verified final directory",
                )
            })?;
        store.transaction(id, |db| published.settle(db, &intake, &info))
    }
}
