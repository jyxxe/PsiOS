//! Authenticate a volume publication when the original atomic rename lost its receipt.
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
use rusqlite::{OptionalExtension, params};
use rustix::fs::{AtFlags, Dir, Mode, OFlags, open, statat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, fs::File, io::Read, os::unix::fs::MetadataExt};

pub(crate) const INSPECT: &str = "volume.inspect_publication";
pub(crate) const CREATE: &str = "volume.create";
pub(crate) const CREATE_FORMAT: u32 = crate::schedule::workflow_overlap::FORMAT + 1;
pub(crate) const FORMAT: u32 = 42;
pub(crate) const DISPOSAL_FORMAT: u32 = crate::events::offsets::FORMAT + 1;
pub(crate) const DISPOSAL_SCHEMA: &str = include_str!("restore_publication/custody.sql");
pub(crate) const PERMIT_MAX_SECONDS: i64 = 86_400;

const INTENT: &str = "RestorePublicationIntent";
const CHECKPOINT: &str = "RestorePublicationCheckpoint";
const NOT_APPLIED: &str = "RestorePublicationNotApplied";
const MAX_RESTORE_STAGES: i64 = 128;

pub(crate) fn is_tool(tool: &str) -> bool {
    matches!(tool, "backup.restore" | super::cloning::TOOL | CREATE)
}

mod creation;
pub(super) use creation::check_empty as check_empty_create;
pub(crate) use creation::migrate as migrate_create;
mod disposal;
mod source;
pub(crate) use disposal::{Disposal, next_disposal, queue_disposal};
use source::Source;
mod verification;
pub(crate) use verification::{clone_origin, verify_clones, verify_creates};
pub(crate) use verification::{protected_stages, verify};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    parent: [String; 2],
    directory: [String; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_name: Option<String>,
}

impl Placement {
    pub(super) fn staged(stage: &staging::Stage) -> Result<Self> {
        let name = stage
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| staging::is_stage_name(name))
            .ok_or_else(|| err("RecoveryRequired", "restore stage name is invalid"))?;
        Ok(Self {
            parent: identity(stage.parent())?,
            directory: identity(stage.directory())?,
            stage_name: Some(name.into()),
        })
    }

    fn stage_name(&self) -> Result<Option<&str>> {
        let name = self.stage_name.as_deref();
        require(
            name.is_none_or(staging::is_stage_name),
            "RecoveryRequired",
            "restore stage name is invalid",
        )?;
        Ok(name)
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

fn held(db: &Db<'_>, op: &Doc, binding: &Source, source: &Doc) -> Result<()> {
    if matches!(binding, Source::Create(_)) {
        // The output is private to this Operation. The original allocation
        // Plan owns its publication, and multiple creates may use one pool.
        return Ok(());
    }
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![source.id, op.id],
        |r| r.get(0),
    )?;
    require(
        held && matches!(source.kind.as_str(), "Backup" | "Volume"),
        "RecoveryRequired",
        "original volume publication source hold is missing",
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
    let (source, target) = Source::load(db.c, &work.op)?;
    held(db, &work.op, &source, &work.target)?;
    let outstanding: i64 = db.c.query_row(
        "SELECT count(*) FROM (
            SELECT 1 FROM queue q JOIN current t ON q.task=t.id
              WHERE q.phase IN ('Dispatched','Unknown') AND (
                   t.spec->>'task_type' IN ('backup.restore','volume.clone') OR (
                   t.spec->>'task_type'='volume.create' AND EXISTS (
                      SELECT 1 FROM current i WHERE i.id='restore-intent-' || q.operation
                   )
              ))
            UNION ALL SELECT 1 FROM restore_stage_disposals LIMIT ?1
         )",
        [MAX_RESTORE_STAGES],
        |row| row.get(0),
    )?;
    require(
        outstanding < MAX_RESTORE_STAGES,
        "QuotaExceeded",
        "restore staging custody limit reached",
    )?;
    require(
        target.reference() == work.target.reference(),
        "RecoveryRequired",
        "publication source changed",
    )?;
    let mut body = json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id});
    source.evidence(&mut body, &target, false);
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        body,
    )?;
    Ok(())
}

pub(crate) fn not_published(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(intent) = db.find(&intent_id(&op.id))? else {
        return Ok(false);
    };
    let (source, target) = Source::load(db.c, op)?;
    let pre_effect = if matches!(source, Source::Create(_)) {
        db.resolve(&op.body["effect_refs"][0])?.body["phase"] == "PermitBound"
    } else {
        true
    };
    Ok(pre_effect
        && belongs(&intent, op, INTENT)
        && source.matches(&intent, &target, false)
        && intent.body.get("checkpoint_ref").is_none()
        && db.find(&checkpoint_id(&op.id))?.is_none())
}

pub(crate) fn record(
    db: &Db<'_>,
    work: &Work,
    receipt: &Value,
    placement: &Placement,
) -> Result<()> {
    let (source, target) = Source::load(db.c, &work.op)?;
    held(db, &work.op, &source, &work.target)?;
    let op = db.of(&work.op.id, "Operation")?;
    require(
        not_published(db, &op)?,
        "Fenced",
        "restore publication intent is unavailable or used",
    )?;
    let intent = db.of(&intent_id(&work.op.id), INTENT)?;
    require(
        target.reference() == work.target.reference(),
        "RecoveryRequired",
        "publication source changed before checkpoint",
    )?;
    source.check_receipt(&work.op, receipt)?;
    let mut checkpoint = json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
                                "receipt":receipt,"placement":placement});
    source.evidence(&mut checkpoint, &target, true);
    if matches!(source, Source::Create(_)) {
        checkpoint["started_effect_ref"] = creation::start_effect(db, work)?;
    }
    let checkpoint = db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        checkpoint,
    )?;
    let mut body = intent.body.clone();
    body["checkpoint_ref"] = checkpoint.reference();
    db.status(&intent, body)?;
    Ok(())
}

fn recorded(db: &Db<'_>, op: &Doc, source: &Doc) -> Result<Doc> {
    let (binding, target) = Source::load(db.c, op)?;
    held(db, op, &binding, source)?;
    let checkpoint = db.find(&checkpoint_id(&op.id))?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "no committed restore publication checkpoint",
        )
    })?;
    require(
        belongs(&checkpoint, op, CHECKPOINT)
            && target.reference() == source.reference()
            && binding.matches(&checkpoint, &target, true),
        "RecoveryRequired",
        "restore publication checkpoint does not bind the original operation and backup",
    )?;
    let intent = db.find(&intent_id(&op.id))?;
    let current_operation = db.of(&op.id, "Operation")?;
    verify_custody(db.c, &current_operation, &checkpoint, intent.as_ref())?;
    binding.check_receipt(op, &checkpoint.body["receipt"])?;
    Ok(checkpoint)
}

fn verify_custody(
    conn: &rusqlite::Connection,
    op: &Doc,
    checkpoint: &Doc,
    intent: Option<&Doc>,
) -> Result<Placement> {
    let placement: Placement = decode(&checkpoint.body["placement"])?;
    let named = placement.stage_name()?.is_some();
    let linked = intent.and_then(|document| document.body.get("checkpoint_ref"));
    require(
        belongs(checkpoint, op, CHECKPOINT)
            && if named {
                intent.is_some_and(|doc| {
                    belongs(doc, op, INTENT) && source::same_binding(doc, checkpoint)
                }) && linked == Some(&checkpoint.reference())
            } else {
                linked.is_none()
                    && checkpoint.body.get("clone_source").is_none()
                    && checkpoint.body.get("create_source").is_none()
            },
        "RecoveryRequired",
        "restore staging custody disagrees with its witnessed intent",
    )?;
    if checkpoint.body.get("create_source").is_some() {
        creation::verify_started(conn, op, checkpoint)?;
    }
    Ok(placement)
}

pub(crate) fn pending(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    let task_id = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "operation has no task"))?;
    let task = db.of(task_id, "Task")?;
    require(
        op.body["outcome_knowledge"] == "Unknown"
            && is_tool(task.spec["task_type"].as_str().unwrap_or_default()),
        "PreconditionFailed",
        "operation is not an unresolved volume restore",
    )?;
    let (binding, original) = Source::load(db.c, op)?;
    if matches!(binding, Source::Create(_)) {
        return recorded(db, op, &original);
    }
    let source = db.of(&original.id, &original.kind)?;
    require(
        source.reference() == original.reference(),
        "RecoveryRequired",
        "publication source differs from the original plan",
    )?;
    recorded(db, op, &source)
}

pub(crate) fn original_create_pool(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    let (binding, pool) = Source::load(db.c, op)?;
    require(
        matches!(binding, Source::Create(_)),
        "RecoveryRequired",
        "operation has no new volume publication source",
    )?;
    recorded(db, op, &pool)?;
    Ok(pool)
}

struct Candidate {
    parent: File,
    root: File,
    data: File,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Publication {
    Published,
    NotApplied,
}

impl Publication {
    fn label(self) -> &'static str {
        match self {
            Self::Published => "Published",
            Self::NotApplied => "NotApplied",
        }
    }
}

fn volume_id(receipt: &Value) -> Result<&str> {
    let volume = receipt["id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "checkpointed volume has no ID"))?;
    filename(volume)?;
    Ok(volume)
}

fn open_parent(config: &Config, placement: &Placement) -> Result<File> {
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
        private_identity(&parent, &placement.parent)?,
        "PreconditionFailed",
        "restore destination parent differs from the checkpoint",
    )?;
    Ok(parent)
}

fn private_identity(directory: &File, expected: &[String; 2]) -> Result<bool> {
    let metadata = directory.metadata()?;
    Ok(metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o077 == 0
        && identity(directory)? == *expected)
}

fn present(parent: &File, name: &str) -> Result<bool> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(e) => Err(std::io::Error::from(e).into()),
    }
}

fn open_candidate(config: &Config, checkpoint: &Doc) -> Result<(Publication, Candidate)> {
    let placement: Placement = decode(&checkpoint.body["placement"])?;
    let receipt = &checkpoint.body["receipt"];
    let volume = volume_id(receipt)?;
    let parent = open_parent(config, &placement)?;
    let stage_name = placement.stage_name()?;
    let final_present = present(&parent, volume)?;
    let stage_present = if let Some(name) = stage_name {
        present(&parent, name)?
    } else {
        false
    };
    let (publication, name) = match (final_present, stage_present, stage_name) {
        (true, false, _) => (Publication::Published, volume),
        (false, true, Some(stage_name)) => (Publication::NotApplied, stage_name),
        _ => {
            return Err(err(
                "PreconditionFailed",
                "neither placement uniquely identifies the original restore",
            ));
        }
    };
    let (root, data) = open_exact(&parent, name, &placement, receipt)?;
    if checkpoint.body.get("create_source").is_some() {
        creation::check_empty(&data)?;
    } else if checkpoint.body.get("clone_source").is_some() {
        super::cloning::root_metadata(&data, "PreconditionFailed")?;
    }
    // Check the alternate name after the anchored opens as well. A competing
    // or substituted directory never authorizes release of the backup hold.
    let alternate = if publication == Publication::Published {
        stage_name
    } else {
        Some(volume)
    };
    if let Some(alternate) = alternate {
        require(
            !present(&parent, alternate)?,
            "PreconditionFailed",
            "competing restore placement is present",
        )?;
    }
    Ok((publication, Candidate { parent, root, data }))
}

fn open_exact(
    parent: &File,
    name: &str,
    placement: &Placement,
    receipt: &Value,
) -> Result<(File, File)> {
    let root = child(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
    require(
        private_identity(&root, &placement.directory)?,
        "PreconditionFailed",
        "restored volume directory differs from the checkpoint",
    )?;
    check_wrapper(&root)?;
    check_marker(&root, receipt)?;
    let data = open_data(&root, receipt)?;
    Ok((root, data))
}

fn check_wrapper(root: &File) -> Result<()> {
    for entry in Dir::read_from(root).map_err(std::io::Error::from)? {
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
    Ok(())
}

fn check_marker(root: &File, receipt: &Value) -> Result<()> {
    let marker = child(root, "identity", OFlags::RDONLY)?;
    regular(&marker.metadata()?)?;
    let mut marker_bytes = Vec::new();
    marker.take(4097).read_to_end(&mut marker_bytes)?;
    require(
        marker_bytes == canonical(&json!([volume_id(receipt)?, receipt["incarnation"]])),
        "PreconditionFailed",
        "restored volume marker differs from the checkpoint",
    )
}

fn open_data(root: &File, receipt: &Value) -> Result<File> {
    let data = child(root, "data", OFlags::RDONLY | OFlags::DIRECTORY)?;
    let [device, inode] = identity(&data)?;
    require(
        receipt["device"] == device && receipt["inode"] == inode,
        "PreconditionFailed",
        "restored data directory differs from the checkpoint",
    )?;
    Ok(data)
}

pub(super) fn observe(work: &Work) -> Result<Value> {
    let checkpoint = work.publication_checkpoint.as_ref().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "inspection lacks its restore publication checkpoint",
        )
    })?;
    let (publication, opened) = open_candidate(&work.cfg, checkpoint)?;
    let before = usage::fingerprint(&opened.data)?;
    let proof = &checkpoint.body["receipt"]["restore_proof"];
    let reopen = || {
        let (current, opened) = open_candidate(&work.cfg, checkpoint)?;
        require(
            current == publication,
            "PreconditionFailed",
            "restore placement changed during inspection",
        )?;
        Ok(opened.data)
    };
    let creating = checkpoint.body.get("create_source").is_some();
    if !creating {
        restore_proof::verify_tree(&opened.data, proof, reopen)?;
    }
    require(
        before == usage::fingerprint(&opened.data)?,
        "Conflict",
        "restored volume changed during content inspection",
    )?;
    opened.data.sync_all()?;
    opened.root.sync_all()?;
    opened.parent.sync_all()?;
    if creating {
        require(
            before == usage::fingerprint(&reopen()?)?,
            "Conflict",
            "new volume changed before publication of the observation",
        )?;
    }
    Ok(
        json!({"operation_ref":work.target.reference(),"checkpoint_ref":checkpoint.reference(),"publication":publication.label(),
        "restore_proof":proof,"metadata_digest":before,"observed_at":now()}),
    )
}

fn catalog(db: &Db<'_>, op: &Doc, source: &Doc, receipt: &Value) -> Result<Value> {
    let (binding, target) = Source::load(db.c, op)?;
    require(
        target.reference() == source.reference(),
        "RecoveryRequired",
        "publication source changed at cataloging",
    )?;
    catalog_volume(
        db,
        &op.owner,
        receipt,
        binding.catalog(&target, &receipt["restore_proof"]),
    )
}

pub(super) fn settle(db: &Db<'_>, work: &Work, receipt: &Value) -> Result<Value> {
    let checkpoint = recorded(db, &work.op, &work.target)?;
    require(
        checkpoint.body["receipt"] == *receipt,
        "RecoveryRequired",
        "restore receipt disagrees with the witnessed candidate",
    )?;
    let (publication, _) = open_candidate(db.cfg, &checkpoint)?;
    require(
        publication == Publication::Published,
        "OutcomeUnknown",
        "restore has no published volume",
    )?;
    catalog(db, &work.op, &work.target, receipt)
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    source: &Doc,
    observation: &Doc,
    proof: &Value,
) -> Result<(bool, Value, Value)> {
    let checkpoint = recorded(db, op, source)?;
    let receipt = &checkpoint.body["receipt"];
    require(
        proof["operation_ref"] == op.reference()
            && proof["checkpoint_ref"] == checkpoint.reference()
            && proof["restore_proof"] == receipt["restore_proof"],
        "OutcomeUnknown",
        "inspection does not verify this restore publication",
    )?;
    let reopen = || {
        open_candidate(db.cfg, &checkpoint).map_err(|error| {
            if source.kind == "StoragePool"
                && matches!(error.0.code.as_str(), "PreconditionFailed" | "Conflict")
            {
                err(
                    "OutcomeUnknown",
                    "new volume no longer matches its inspection",
                )
            } else {
                error
            }
        })
    };
    let (publication, candidate) = reopen()?;
    require(
        proof["metadata_digest"] == usage::fingerprint(&candidate.data)?
            && (proof["publication"] == publication.label()
                || (proof["publication"].is_null()
                    && publication == Publication::Published
                    && checkpoint.body["placement"]["stage_name"].is_null())),
        "OutcomeUnknown",
        "restored volume changed after inspection",
    )?;
    require(
        reopen()?.0 == publication,
        "OutcomeUnknown",
        "restore placement changed after reinspection",
    )?;
    let applied = publication == Publication::Published;
    let result = if applied {
        catalog(db, op, source, receipt)?
    } else if source.kind == "StoragePool" {
        json!({"pool_ref":source.reference(),"volume_id":receipt["id"],"publication":"NotApplied"})
    } else if source.kind == "Backup" {
        json!({"backup_ref":source.reference(),"volume_id":receipt["id"],"publication":"NotApplied"})
    } else {
        json!({"source_volume_ref":source.reference(),"volume_id":receipt["id"],"publication":"NotApplied"})
    };
    Ok((
        applied,
        result,
        json!({"observation_ref":observation.reference(),
        "checkpoint_ref":checkpoint.reference(),"conclusion":if applied {"RestorePublicationConfirmed"} else {NOT_APPLIED}}),
    ))
}
