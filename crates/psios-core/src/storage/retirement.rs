//! Retire an approved filesystem identity before asynchronous physical reclamation.
use super::{
    MAX_MANIFEST, child, copy_hash, filename, open_manifest, registered_root, regular,
    remove_contents, same_source, unknown,
};
use crate::{
    Config, Error, Result, after,
    config::private_dir,
    db::{Db, Doc},
    decode, err,
    host::{Action, Work},
    now, policy, require,
};
use psios_api::canonical;
use rusqlite::{OptionalExtension, params};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RenameFlags, open, renameat_with, statat, unlinkat,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, sink},
    os::unix::fs::MetadataExt,
};

pub(crate) const FORMAT: u32 = 39;
pub(crate) const SCHEMA: &str = include_str!("retirement.sql");
const KIND: &str = "StorageRetirement";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    device: String,
    inode: String,
}

impl Identity {
    fn new(device: u64, inode: u64) -> Self {
        Self {
            device: device.to_string(),
            inode: inode.to_string(),
        }
    }

    fn file(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        Ok(Self::new(m.dev(), m.ino()))
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Location {
    parent: Identity,
    directory: Identity,
}

pub(crate) fn deletion(action: &Action) -> bool {
    matches!(action, Action::Delete(_) | Action::DeleteBackup(_))
}

fn id(operation: &str) -> String {
    format!("storage-retirement-{operation}")
}

fn retired_name(operation: &str) -> String {
    format!(".retired-{operation}")
}

fn open_parent(config: &Config, kind: &str) -> Result<File> {
    let dirname = match kind {
        "Backup" => "backups",
        "Volume" => "volumes",
        _ => return Err(err("RecoveryRequired", "unknown retired storage kind")),
    };
    let path = config.state_dir.join(dirname);
    private_dir(&path, false)?;
    let file = File::from(
        open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let m = file.metadata()?;
    require(
        m.uid() == rustix::process::geteuid().as_raw() && m.mode() & 0o077 == 0,
        "PermissionDenied",
        "storage parent is not a private owned directory",
    )?;
    Ok(file)
}

fn entry(parent: &File, name: &str) -> Result<Option<Identity>> {
    filename(name)?;
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(info) => {
            require(
                FileType::from_raw_mode(info.st_mode) == FileType::Directory,
                "PreconditionFailed",
                "storage deletion entry is no longer a directory",
            )?;
            Ok(Some(Identity::new(info.st_dev, info.st_ino)))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(std::io::Error::from(e).into()),
    }
}

fn check_target(root: &File, target: &Doc) -> Result<()> {
    if target.kind == "Volume" {
        super::entry_retirement::guard_volume_retirement(root)?;
        let marker = child(root, "identity", OFlags::RDONLY)?;
        regular(&marker.metadata()?)?;
        let mut identity = Vec::new();
        marker.take(4097).read_to_end(&mut identity)?;
        require(
            identity == canonical(&json!([target.id, target.body["data_incarnation"]])),
            "PreconditionFailed",
            "volume identity mismatch during deletion",
        )?;
        return registered_root(
            target,
            &child(root, "data", OFlags::RDONLY | OFlags::DIRECTORY)?.metadata()?,
        );
    }
    let mut manifest = open_manifest(root)?;
    let before = manifest.metadata()?;
    let (bytes, digest) = copy_hash(&mut manifest, &mut sink(), MAX_MANIFEST)?;
    require(
        bytes == before.len()
            && target.body["manifest_digest"] == digest
            && same_source(&before, &manifest.metadata()?),
        "PreconditionFailed",
        "backup manifest integrity failed during deletion",
    )
}

fn current_hold(db: &Db<'_>, op: &Doc, target: &Doc) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![target.id, op.id],
        |row| row.get(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "original storage deletion hold unavailable",
    )
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    current_hold(db, &work.op, &work.target)?;
    db.create(
        KIND,
        &id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id,"target_ref":work.target.reference(),
            "plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id}),
        json!({"lifecycle":"Preparing"}),
    )?;
    Ok(())
}

fn record(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    let retirement = db.find(&id(&op.id))?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "storage deletion checkpoint unavailable",
        )
    })?;
    require(
        retirement.kind == KIND
            && retirement.owner == op.owner
            && retirement.spec["operation_id"] == op.id
            && retirement.spec["plan_ref"] == op.body["plan_ref"]
            && retirement.spec["effect_id"] == op.body["effect_refs"][0]["resource_id"],
        "RecoveryRequired",
        "storage retirement does not bind the original effect",
    )?;
    Ok(retirement)
}

pub(crate) fn not_prepared(db: &Db<'_>, op: &Doc) -> Result<bool> {
    match db.find(&id(&op.id))? {
        Some(_) => Ok(record(db, op)?.body["lifecycle"] == "Preparing"),
        None => Ok(false),
    }
}

pub(crate) fn abandon(db: &Db<'_>, op: &Doc) -> Result<()> {
    if db.find(&id(&op.id))?.is_some() {
        let retirement = record(db, op)?;
        if matches!(
            retirement.body["lifecycle"].as_str(),
            Some("Preparing" | "Prepared")
        ) {
            db.status(
                &retirement,
                json!({"lifecycle":"NotApplied","settled_at":now()}),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn prepare(db: &Db<'_>, work: &Work, identity: &Location) -> Result<()> {
    current_hold(db, &work.op, &work.target)?;
    let retirement = record(db, &work.op)?;
    require(
        retirement.body["lifecycle"] == "Preparing"
            && retirement.spec["target_ref"] == work.target.reference(),
        "Fenced",
        "storage deletion permit already used or target changed",
    )?;
    db.status(
        &retirement,
        json!({"lifecycle":"Prepared","location":identity,"prepared_at":now()}),
    )?;
    Ok(())
}

pub(crate) fn retire(
    work: &Work,
    checkpoint: impl FnOnce(&Location) -> Result<()>,
) -> Result<Value> {
    let parent = open_parent(&work.cfg, &work.target.kind)?;
    let name = &work.target.id;
    let root = child(&parent, name.as_str(), OFlags::RDONLY | OFlags::DIRECTORY)?;
    let metadata = root.metadata()?;
    require(
        metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o077 == 0,
        "PermissionDenied",
        "storage deletion requires a private owned directory",
    )?;
    check_target(&root, &work.target)?;
    let location = Location {
        parent: Identity::file(&parent)?,
        directory: Identity::file(&root)?,
    };
    let retired = retired_name(&work.op.id);
    checkpoint(&location)?;
    if work.target.kind == "Volume" {
        super::entry_retirement::guard_volume_retirement(&root)?;
    }
    require(
        Identity::file(&open_parent(&work.cfg, &work.target.kind)?)? == location.parent
            && entry(&parent, name)?.as_ref() == Some(&location.directory)
            && entry(&parent, &retired)?.is_none(),
        "PreconditionFailed",
        "storage deletion namespace changed before the effect",
    )?;
    renameat_with(&parent, name, &parent, &retired, RenameFlags::NOREPLACE).map_err(unknown)?;
    require(
        entry(&parent, name).map_err(unknown)?.is_none()
            && entry(&parent, &retired).map_err(unknown)?.as_ref() == Some(&location.directory),
        "OutcomeUnknown",
        "storage retirement identity unconfirmed after rename",
    )?;
    parent.sync_all().map_err(unknown)?;
    Ok(json!(location))
}

fn prepared(db: &Db<'_>, op: &Doc, target: &Doc) -> Result<Doc> {
    current_hold(db, op, target)?;
    let retirement = record(db, op)?;
    require(
        retirement.body["lifecycle"] == "Prepared"
            && retirement.spec["target_ref"] == target.reference(),
        "RecoveryRequired",
        "no prepared retirement for this storage revision",
    )?;
    Ok(retirement)
}

pub(crate) fn settle(db: &Db<'_>, op: &Doc, target: &Doc, location: &Value) -> Result<Value> {
    let retirement = prepared(db, op, target)?;
    require(
        retirement.body["location"] == *location,
        "RecoveryRequired",
        "storage receipt does not match its prepared directory",
    )?;
    let mut body = target.body.clone();
    body["lifecycle"] = json!("Deleted");
    if target.kind == "Backup" {
        body["deleted_at"] = json!(now());
    }
    let deleted = db.status(target, body)?;
    let retirement = db.status(
        &retirement,
        json!({"lifecycle":"PendingCleanup","location":location,
        "deleted_ref":deleted.reference(),"retired_at":now()}),
    )?;
    db.c.execute(
        "INSERT INTO storage_reclamations VALUES(?1,?2)",
        params![retirement.id, now()],
    )?;
    Ok(if target.kind == "Backup" {
        json!(deleted.reference())
    } else {
        json!({"volume_ref":deleted.reference(),"receipt":{"retirement_ref":retirement.reference()}})
    })
}

pub(crate) fn reconcile(db: &Db<'_>, op: &Doc, target: &Doc) -> Result<(bool, Value, Value)> {
    let retirement = prepared(db, op, target)?;
    let location: Location = decode(&retirement.body["location"])?;
    let parent = open_parent(db.cfg, &target.kind)?;
    require(
        Identity::file(&parent)? == location.parent,
        "OutcomeUnknown",
        "storage parent identity changed since the approved deletion",
    )?;
    let source = entry(&parent, &target.id)?;
    let retired = entry(&parent, &retired_name(&op.id))?;
    let completed = match (source, retired) {
        (None, Some(retired)) if retired == location.directory => true,
        (Some(source), None) if source == location.directory => false,
        _ => {
            return Err(err(
                "OutcomeUnknown",
                "storage retirement has no matching directory evidence",
            ));
        }
    };
    parent.sync_all()?;
    let result = if completed {
        settle(db, op, target, &retirement.body["location"])?
    } else {
        json!({"deleted":false})
    };
    Ok((
        completed,
        result,
        json!({"checkpoint_ref":retirement.reference(),
        "conclusion":if completed {"RetirementConfirmed"} else {"RetirementNotApplied"}}),
    ))
}

pub(crate) struct Reclamation {
    record: Doc,
    target: Doc,
    operation: String,
    location: Location,
}

pub(crate) enum PhysicalResult {
    Removed,
    AlreadyAbsent,
}

pub(crate) fn next(db: &Db<'_>) -> Result<Option<Reclamation>> {
    let id = db.c.query_row(
        "SELECT retirement FROM storage_reclamations WHERE ready_after<=?1 ORDER BY ready_after,retirement LIMIT 1",
        [now()], |row| row.get::<_, String>(0),
    ).optional()?;
    let Some(id) = id else { return Ok(None) };
    let job = db.of(&id, KIND)?;
    let operation_id = job.spec["operation_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "reclamation has no deletion operation"))?;
    let operation = db.of(operation_id, "Operation")?;
    let record = record(db, &operation)?;
    let target = db.resolve(&record.body["deleted_ref"])?;
    require(
        job.id == record.id
            && record.body["lifecycle"] == "PendingCleanup"
            && operation.body["outcome_knowledge"] == "Known"
            && target.body["lifecycle"] == "Deleted"
            && record.spec["target_ref"]["resource_id"] == target.id
            && db.get(&target.id)?.reference() == target.reference(),
        "RecoveryRequired",
        "reclamation is not bound to a committed deletion",
    )?;
    let location = decode(&record.body["location"])?;
    Ok(Some(Reclamation {
        record,
        target,
        operation: operation.id,
        location,
    }))
}

impl Reclamation {
    pub(crate) fn reclaim(&self, config: &Config) -> Result<PhysicalResult> {
        let parent = open_parent(config, &self.target.kind)?;
        let name = retired_name(&self.operation);
        require(
            Identity::file(&parent)? == self.location.parent
                && entry(&parent, &self.target.id)?.is_none(),
            "PreconditionFailed",
            "reclamation parent or retired resource pathname changed",
        )?;
        let result = if let Some(found) = entry(&parent, &name)? {
            require(
                found == self.location.directory,
                "PreconditionFailed",
                "reclamation directory changed",
            )?;
            let root = child(&parent, name.as_str(), OFlags::RDONLY | OFlags::DIRECTORY)?;
            require(
                Identity::file(&root)? == found && root.metadata()?.mode() & 0o077 == 0,
                "PreconditionFailed",
                "reclamation requires the private retired directory",
            )?;
            if self.target.kind == "Volume" {
                super::entry_retirement::guard_volume_retirement(&root)?;
            }
            remove_contents(&root, 0)?;
            require(
                entry(&parent, &name)?.as_ref() == Some(&found),
                "PreconditionFailed",
                "retired directory changed during reclamation",
            )?;
            unlinkat(&parent, &name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
            PhysicalResult::Removed
        } else {
            PhysicalResult::AlreadyAbsent
        };
        parent.sync_all()?;
        Ok(result)
    }

    pub(crate) fn finish(&self, db: &Db<'_>, result: Result<PhysicalResult>) -> Result<()> {
        let mut body = self.record.body.clone();
        match result {
            Ok(result) => {
                let observation = match result {
                    PhysicalResult::Removed => "DirectoryRemoved",
                    PhysicalResult::AlreadyAbsent => "DirectoryAlreadyAbsent",
                };
                body.as_object_mut().unwrap().remove("last_error");
                body["lifecycle"] = json!("Reclaimed");
                body["reclaimed_at"] = json!(now());
                body["observation"] = json!(observation);
                db.status(&self.record, body)?;
                db.c.execute(
                    "DELETE FROM storage_reclamations WHERE retirement=?1",
                    [&self.record.id],
                )?;
                db.audit(
                    policy::SYSTEM,
                    "Storage.Reclaim",
                    Some(&self.target.id),
                    observation,
                )?;
            }
            Err(Error(error)) => {
                if body["last_error"] != error.code {
                    body["last_error"] = json!(error.code);
                    db.status(&self.record, body)?;
                    db.audit(
                        policy::SYSTEM,
                        "Storage.Reclaim",
                        Some(&self.target.id),
                        &error.code,
                    )?;
                }
                db.c.execute(
                    "UPDATE storage_reclamations SET ready_after=?1 WHERE retirement=?2",
                    params![after(30), self.record.id],
                )?;
            }
        }
        Ok(())
    }
}
