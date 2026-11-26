//! Recover an interrupted backup by following the original permit and publication placement.
use super::*;
use placement::{Publication, backup_id, open_candidate};

mod custody;
mod placement;
pub(crate) use custody::{
    DISPOSAL_SCHEMA, Disposal, checkpointed, next_disposal, protected_stages, queue_disposal,
    verify,
};
pub(crate) use placement::Placement;

pub(crate) const INSPECT: &str = "backup.inspect_publication";
// Earlier daemons reclaim every .stage-* entry in the backup directory.
pub(crate) const FORMAT: u32 = 123;
const INTENT: &str = "BackupPublicationIntent";
const CHECKPOINT: &str = "BackupPublicationCheckpoint";
const NOT_APPLIED: &str = "BackupPublicationNotApplied";
const NAMED_STAGE: &str = "NamedStageV1";

fn checkpoint_id(operation: &str) -> String {
    format!("backup-publication-{operation}")
}

fn intent_id(operation: &str) -> String {
    format!("backup-intent-{operation}")
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.op, &work.target)?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"], "effect_id":work.effect.id, "protocol":NAMED_STAGE}),
    )?;
    Ok(())
}

pub(crate) fn not_published(db: &Db<'_>, op: &Doc) -> Result<bool> {
    // Only a dispatch by this protocol proves that publication could not
    // have started before a checkpoint was committed to the witnessed DB.
    let Some(intent) = db.find(&intent_id(&op.id))? else {
        return Ok(false);
    };
    Ok(belongs(&intent, op, INTENT)
        && intent.body.get("checkpoint_ref").is_none()
        && db.find(&checkpoint_id(&op.id))?.is_none()
        && !custody::checkpointed(db, op)?)
}

fn belongs(document: &Doc, op: &Doc, kind: &str) -> bool {
    document.kind == kind
        && document.owner == op.owner
        && document.spec["operation_id"] == op.id
        && document.body["plan_ref"] == op.body["plan_ref"]
        && document.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
}

fn check_receipt(op: &Doc, receipt: &Value) -> Result<()> {
    require(
        backup_id(receipt)? == format!("backup-{}", op.id)
            && receipt["manifest_digest"]
                .as_str()
                .is_some_and(file_integrity::sha256_format)
            && receipt["entries"]
                .as_u64()
                .is_some_and(|n| n <= MAX_ENTRIES as u64)
            && receipt["bytes"]
                .as_str()
                .and_then(|bytes| bytes.parse::<u64>().ok())
                .is_some_and(|bytes| bytes <= crate::MAX_BACKUP_BYTES),
        "RecoveryRequired",
        "backup receipt differs from its original publication",
    )
}

fn held(db: &Db<'_>, op: &Doc, volume: &Doc) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, op.id],
        |r| r.get(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "original backup volume hold missing",
    )
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
        "backup publication intent is unavailable or already used",
    )?;
    let intent = db.of(&intent_id(&work.op.id), INTENT)?;
    require(
        intent.body["protocol"] == NAMED_STAGE,
        "RecoveryRequired",
        "backup publication intent does not support named stage custody",
    )?;
    check_receipt(&work.op, receipt)?;
    let checkpoint = db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.task.owner,
        json!({"operation_id":work.op.id}),
        json!({
            "plan_ref":work.op.body["plan_ref"],
            "effect_id":work.effect.id,
            "source_ref":work.target.reference(),
            "data_incarnation":work.target.body["data_incarnation"],
            "directory": placement.directory,
            "placement": placement,
            "intent_ref": intent.reference(),
            "receipt":receipt
        }),
    )?;
    custody::reserve(db, &work.op, &checkpoint, placement)?;
    let mut body = intent.body.clone();
    body["checkpoint_ref"] = checkpoint.reference();
    db.status(&intent, body)?;
    Ok(())
}

fn recorded(db: &Db<'_>, op: &Doc, volume: &Doc) -> Result<Doc> {
    held(db, op, volume)?;
    let c = db.find(&checkpoint_id(&op.id))?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "no committed backup publication checkpoint",
        )
    })?;
    require(
        belongs(&c, op, CHECKPOINT)
            && c.body["source_ref"] == volume.reference()
            && c.body["data_incarnation"] == volume.body["data_incarnation"],
        "RecoveryRequired",
        "backup publication checkpoint does not bind this operation and volume",
    )?;
    check_receipt(op, &c.body["receipt"])?;
    custody::check_active_checkpoint(db.c, op, &c)?;
    Ok(c)
}

pub(crate) fn pending(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    let task = db.of(op.spec["task_id"].as_str().unwrap(), "Task")?;
    require(
        op.body["outcome_knowledge"] == "Unknown" && task.spec["task_type"] == "backup.create",
        "PreconditionFailed",
        "operation is not an unresolved backup creation",
    )?;
    let plan = db.resolve(&op.body["plan_ref"])?;
    let original = db.resolve(&plan.body["allowed_effects"][0]["resource_ref"])?;
    let volume = db.of(&original.id, "Volume")?;
    require(
        volume.reference() == original.reference(),
        "RecoveryRequired",
        "source volume differs from the backup execution plan",
    )?;
    recorded(db, op, &volume)
}

type Member = [Value; 6];
type MemberSet = BTreeMap<String, Member>;

pub(super) fn file_set(root: &File) -> Result<(String, usize)> {
    let members = members(root)?;
    Ok((member_digest(&members), members.len()))
}

fn member_name(name: &str) -> bool {
    name == "manifest.json"
        || name
            .strip_prefix("blob-")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|n| n < MAX_ENTRIES && name == format!("blob-{n}"))
}

fn member_digest(members: &MemberSet) -> String {
    sha256(&canonical(&json!(members)))
}

fn member(root: &File, device: u64, name: &str) -> Result<Member> {
    let m = statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
    require(
        FileType::from_raw_mode(m.st_mode) == FileType::RegularFile
            && m.st_nlink == 1
            && m.st_dev == device,
        "PreconditionFailed",
        "published backup member changed type or filesystem",
    )?;
    Ok([
        json!(m.st_ino),
        json!(m.st_size),
        json!(m.st_mtime),
        json!(m.st_mtime_nsec),
        json!(m.st_ctime),
        json!(m.st_ctime_nsec),
    ])
}

fn members(root: &File) -> Result<MemberSet> {
    let device = root.metadata()?.dev();
    let mut members = BTreeMap::new();
    for entry in Dir::read_from(root).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| err("PreconditionFailed", "invalid name in published backup"))?;
        if matches!(name, "." | "..") {
            continue;
        }
        require(
            members.len() <= MAX_ENTRIES && member_name(name),
            "PreconditionFailed",
            "unexpected member in published backup",
        )?;
        members.insert(name.to_owned(), member(root, device, name)?);
    }
    Ok(members)
}

pub(super) fn observe(work: &Work) -> Result<Value> {
    let c = work
        .publication_checkpoint
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "inspection lacks its backup checkpoint"))?;
    let placement = Placement::from_checkpoint(c)?;
    let (publication, parent, root) = open_candidate(&work.cfg, c, placement.as_ref())?;
    let (before, files) = file_set(&root)?;
    if publication == Publication::NotApplied {
        require(
            placement
                .as_ref()
                .is_some_and(|place| place.file_set_digest == before),
            "PreconditionFailed",
            "unpublished backup members differ from the captured stage",
        )?;
    }
    let receipt = &c.body["receipt"];
    let manifest = authority_manifest(
        &open_manifest(&root)?,
        receipt["id"].as_str().unwrap(),
        &c.body["source_ref"],
        receipt,
    )?;
    require(
        files
            == 1 + manifest
                .entries
                .iter()
                .filter(|entry| entry.digest.is_some())
                .count(),
        "PreconditionFailed",
        "backup contains unexpected files",
    )?;
    let mut report = inspect_members(
        &root,
        &manifest,
        &receipt["manifest_digest"],
        None,
        crate::MAX_BACKUP_BYTES,
    )?;
    require(
        before == file_set(&root)?.0,
        "PreconditionFailed",
        "backup file set changed during inspection",
    )?;
    // The data files were synced during staging. Seal the observed name if
    // the original process lost power between the rename and its directory sync.
    root.sync_all()?;
    parent.sync_all()?;
    if let Some(placement) = &placement {
        let (current, _, root) = open_candidate(&work.cfg, c, Some(placement))?;
        require(
            current == publication && file_set(&root)?.0 == before,
            "PreconditionFailed",
            "backup placement or content changed during inspection",
        )?;
        report["publication"] = json!(publication.label());
    }
    report["file_set_digest"] = json!(before);
    report["checkpoint_ref"] = c.reference();
    report["operation_ref"] = work.target.reference();
    Ok(report)
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    volume: &Doc,
    observation: &Doc,
    proof: &Value,
) -> Result<(bool, Value, Value)> {
    let c = recorded(db, op, volume)?;
    let receipt = &c.body["receipt"];
    require(
        proof["operation_ref"] == op.reference()
            && proof["checkpoint_ref"] == c.reference()
            && proof["manifest_digest"] == receipt["manifest_digest"]
            && proof["entries"] == receipt["entries"]
            && proof["bytes"] == receipt["bytes"],
        "OutcomeUnknown",
        "observation does not verify the original backup publication",
    )?;
    let placement = Placement::from_checkpoint(&c)?;
    let (publication, _, root) = open_candidate(db.cfg, &c, placement.as_ref())?;
    let member_set = members(&root)?;
    let digest = member_digest(&member_set);
    require(
        proof["file_set_digest"] == digest
            && match &placement {
                Some(place) => {
                    proof["publication"] == publication.label()
                        && (publication == Publication::Published
                            || place.file_set_digest == digest)
                }
                None => proof.get("publication").is_none() && publication == Publication::Published,
            },
        "OutcomeUnknown",
        "backup file set changed after inspection",
    )?;
    if let Some(placement) = &placement {
        let (current, _, root) = open_candidate(db.cfg, &c, Some(placement))?;
        require(
            current == publication && proof["file_set_digest"] == file_set(&root)?.0,
            "OutcomeUnknown",
            "backup placement changed after reinspection",
        )?;
    }
    let applied = publication == Publication::Published;
    let (result, conclusion) = if applied {
        (catalog(db, op, volume, &c)?, "BackupPublicationConfirmed")
    } else {
        (
            json!({"source_volume_ref":volume.reference(),"backup_id":receipt["id"],"publication":"NotApplied"}),
            NOT_APPLIED,
        )
    };
    let mut evidence = json!({
        "observation_ref":observation.reference(),
        "checkpoint_ref":c.reference(),
        "conclusion":conclusion
    });
    if !applied {
        evidence["file_set_digest"] = proof["file_set_digest"].clone();
        evidence["members"] = json!(member_set);
    }
    Ok((applied, result, evidence))
}

pub(super) fn settle(db: &Db<'_>, op: &Doc, volume: &Doc, receipt: &Value) -> Result<Value> {
    let c = recorded(db, op, volume)?;
    require(
        c.body["receipt"] == *receipt,
        "RecoveryRequired",
        "backup receipt disagrees with the committed checkpoint",
    )?;
    let placement = Placement::from_checkpoint(&c)?;
    let (publication, _, root) = open_candidate(db.cfg, &c, placement.as_ref())?;
    require(
        publication == Publication::Published
            && match &placement {
                Some(place) => place.file_set_digest == file_set(&root)?.0,
                None => true,
            },
        "OutcomeUnknown",
        "published backup does not match the witnessed capture",
    )?;
    catalog(db, op, volume, &c)
}

fn catalog(db: &Db<'_>, op: &Doc, volume: &Doc, checkpoint: &Doc) -> Result<Value> {
    let receipt = &checkpoint.body["receipt"];
    if Placement::from_checkpoint(checkpoint)?.is_some() {
        custody::release_published(db, op, checkpoint)?;
    }
    let b = json!({
        "lifecycle": "Stored",
        "manifest_digest":receipt["manifest_digest"],
        "entries":receipt["entries"],
        "bytes":receipt["bytes"],
        "consistency": "crash_consistent",
        "failure_domain": "local-filesystem",
        "coordination": "exclusive offline volume hold"
    });
    Ok(db
        .create(
            "Backup",
            receipt["id"].as_str().unwrap(),
            &op.owner,
            json!({"source_ref":volume.reference(),"source_volume":volume.id}),
            b,
        )?
        .reference())
}
