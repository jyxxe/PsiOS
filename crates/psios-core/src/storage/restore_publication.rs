//! Authenticate a restored volume when the original atomic rename lost its receipt.
use super::{catalog_volume, child, filename, regular, restore_proof, staging, usage};
use crate::{
    Config, Result,
    config::private_dir,
    db::{Db, Doc},
    decode, err,
    host::Work,
    now, require,
};
use psios_api::canonical;
use rusqlite::params;
use rustix::fs::{Dir, Mode, OFlags, open};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::File, io::Read, os::unix::fs::MetadataExt};

pub(crate) const INSPECT: &str = "volume.inspect_publication";
pub(crate) const FORMAT: u32 = 42;
pub(crate) const PERMIT_MAX_SECONDS: i64 = 86_400;

const INTENT: &str = "RestorePublicationIntent";
const CHECKPOINT: &str = "RestorePublicationCheckpoint";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    parent: [String; 2],
    directory: [String; 2],
}

impl Placement {
    pub(super) fn staged(stage: &staging::Stage) -> Result<Self> {
        Ok(Self {
            parent: identity(stage.parent())?,
            directory: identity(stage.directory())?,
        })
    }
}

fn identity(file: &File) -> Result<[String; 2]> {
    let m = file.metadata()?;
    Ok([m.dev().to_string(), m.ino().to_string()])
}

fn intent_id(operation: &str) -> String {
    format!("restore-intent-{operation}")
}

fn checkpoint_id(operation: &str) -> String {
    format!("restore-publication-{operation}")
}

fn held(db: &Db<'_>, op: &Doc, backup: &Doc) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![backup.id, op.id],
        |r| r.get(0),
    )?;
    require(
        held && backup.kind == "Backup",
        "RecoveryRequired",
        "original restore backup hold is missing",
    )
}

fn belongs(record: &Doc, op: &Doc, kind: &str) -> bool {
    record.kind == kind
        && record.owner == op.owner
        && record.spec["operation_id"] == op.id
        && record.body["plan_ref"] == op.body["plan_ref"]
        && record.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.op, &work.target)?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
               "backup_ref":work.target.reference()}),
    )?;
    Ok(())
}

pub(crate) fn not_published(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(intent) = db.find(&intent_id(&op.id))? else {
        return Ok(false);
    };
    let plan = db.resolve(&op.body["plan_ref"])?;
    Ok(belongs(&intent, op, INTENT)
        && intent.body["backup_ref"] == plan.body["allowed_effects"][0]["resource_ref"]
        && db.find(&checkpoint_id(&op.id))?.is_none())
}

pub(crate) fn record(
    db: &Db<'_>,
    work: &Work,
    receipt: &Value,
    placement: &Placement,
) -> Result<()> {
    held(db, &work.op, &work.target)?;
    require(
        not_published(db, &work.op)?,
        "Fenced",
        "restore publication intent is unavailable or used",
    )?;
    db.create(CHECKPOINT, &checkpoint_id(&work.op.id), &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
               "backup_ref":work.target.reference(),"manifest_digest":work.target.body["manifest_digest"],
               "receipt":receipt,"placement":placement}))?;
    Ok(())
}

fn recorded(db: &Db<'_>, op: &Doc, backup: &Doc) -> Result<Doc> {
    held(db, op, backup)?;
    let checkpoint = db.find(&checkpoint_id(&op.id))?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "no committed restore publication checkpoint",
        )
    })?;
    require(
        belongs(&checkpoint, op, CHECKPOINT)
            && checkpoint.body["backup_ref"] == backup.reference()
            && checkpoint.body["manifest_digest"] == backup.body["manifest_digest"],
        "RecoveryRequired",
        "restore publication checkpoint does not bind the original operation and backup",
    )?;
    Ok(checkpoint)
}

pub(crate) fn pending(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    let task_id = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "operation has no task"))?;
    let task = db.of(task_id, "Task")?;
    require(
        op.body["outcome_knowledge"] == "Unknown" && task.spec["task_type"] == "backup.restore",
        "PreconditionFailed",
        "operation is not an unresolved volume restore",
    )?;
    let plan = db.resolve(&op.body["plan_ref"])?;
    let original = db.resolve(&plan.body["allowed_effects"][0]["resource_ref"])?;
    let backup = db.of(&original.id, "Backup")?;
    require(
        backup.reference() == original.reference(),
        "RecoveryRequired",
        "source backup differs from the original restore plan",
    )?;
    recorded(db, op, &backup)
}

struct Candidate {
    parent: File,
    root: File,
    data: File,
}

fn open_candidate(config: &Config, checkpoint: &Doc) -> Result<Candidate> {
    let placement: Placement = decode(&checkpoint.body["placement"])?;
    let receipt = &checkpoint.body["receipt"];
    let volume = receipt["id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "checkpointed volume has no ID"))?;
    filename(volume)?;
    let path = config.state_dir.join("volumes");
    private_dir(&path, false)?;
    let parent = File::from(
        open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    require(
        identity(&parent)? == placement.parent,
        "PreconditionFailed",
        "restore destination parent differs from the checkpoint",
    )?;
    let root = child(&parent, volume, OFlags::RDONLY | OFlags::DIRECTORY)?;
    require(
        identity(&root)? == placement.directory,
        "PreconditionFailed",
        "restored volume directory differs from the checkpoint",
    )?;
    for entry in Dir::read_from(&root).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        require(
            matches!(
                entry.file_name().to_bytes(),
                b"." | b".." | b"data" | b"identity"
            ),
            "PreconditionFailed",
            "restored volume wrapper contains an unexpected entry",
        )?;
    }
    let marker = child(&root, "identity", OFlags::RDONLY)?;
    regular(&marker.metadata()?)?;
    let mut marker_bytes = Vec::new();
    marker.take(4097).read_to_end(&mut marker_bytes)?;
    require(
        marker_bytes == canonical(&json!([volume, receipt["incarnation"]])),
        "PreconditionFailed",
        "restored volume marker differs from the checkpoint",
    )?;
    let data = child(&root, "data", OFlags::RDONLY | OFlags::DIRECTORY)?;
    let [device, inode] = identity(&data)?;
    require(
        receipt["device"] == device && receipt["inode"] == inode,
        "PreconditionFailed",
        "restored data directory differs from the checkpoint",
    )?;
    Ok(Candidate { parent, root, data })
}

pub(super) fn observe(work: &Work) -> Result<Value> {
    let checkpoint = work.publication_checkpoint.as_ref().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "inspection lacks its restore publication checkpoint",
        )
    })?;
    let opened = open_candidate(&work.cfg, checkpoint)?;
    let before = usage::fingerprint(&opened.data)?;
    let proof = &checkpoint.body["receipt"]["restore_proof"];
    restore_proof::verify_tree(&opened.data, proof, || {
        Ok(open_candidate(&work.cfg, checkpoint)?.data)
    })?;
    require(
        before == usage::fingerprint(&opened.data)?,
        "Conflict",
        "restored volume changed during content inspection",
    )?;
    opened.data.sync_all()?;
    opened.root.sync_all()?;
    opened.parent.sync_all()?;
    Ok(
        json!({"operation_ref":work.target.reference(),"checkpoint_ref":checkpoint.reference(),
        "restore_proof":proof,"metadata_digest":before,"observed_at":now()}),
    )
}

fn catalog(db: &Db<'_>, op: &Doc, backup: &Doc, receipt: &Value) -> Result<Value> {
    catalog_volume(
        db,
        &op.owner,
        receipt,
        json!({
            "restored_from":backup.reference(),"storage_restore_phase":"AwaitingActivation",
            "restore_proof":receipt["restore_proof"],
        }),
    )
}

pub(super) fn settle(db: &Db<'_>, work: &Work, receipt: &Value) -> Result<Value> {
    let checkpoint = recorded(db, &work.op, &work.target)?;
    require(
        checkpoint.body["receipt"] == *receipt,
        "RecoveryRequired",
        "restore receipt disagrees with the witnessed candidate",
    )?;
    open_candidate(db.cfg, &checkpoint)?;
    catalog(db, &work.op, &work.target, receipt)
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    backup: &Doc,
    observation: &Doc,
    proof: &Value,
) -> Result<(Value, Value)> {
    let checkpoint = recorded(db, op, backup)?;
    let receipt = &checkpoint.body["receipt"];
    require(
        proof["operation_ref"] == op.reference()
            && proof["checkpoint_ref"] == checkpoint.reference()
            && proof["restore_proof"] == receipt["restore_proof"],
        "OutcomeUnknown",
        "inspection does not verify this restore publication",
    )?;
    let candidate = open_candidate(db.cfg, &checkpoint)?;
    require(
        proof["metadata_digest"] == usage::fingerprint(&candidate.data)?,
        "OutcomeUnknown",
        "restored volume changed after inspection",
    )?;
    Ok((
        catalog(db, op, backup, receipt)?,
        json!({"observation_ref":observation.reference(),
        "checkpoint_ref":checkpoint.reference(),"conclusion":"RestorePublicationConfirmed"}),
    ))
}
